//! A Docker Registry HTTP API v2 client.
//!
//! Implements the parts of the protocol skopeo needs: bearer/basic
//! authentication, manifest pull/push/delete, blob pull/push, tag listing and
//! monolithic blob uploads. It is intentionally synchronous; copy operations
//! are sequential and the CLI has no need for async.

use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use sha2::Digest as _;
use ureq::http::Response;
use ureq::tls::{RootCerts, TlsConfig};
use ureq::{Agent, Body, BodyReader};
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{self};
use crate::digest::Digest;
use crate::manifest::{self, ManifestBytes};
use crate::options::ImageOptions;
use crate::util;

/// An error carrying an HTTP status code, so callers can distinguish
/// `403 Forbidden` (tag listing blocked) from real failures.
#[derive(Debug)]
pub struct HttpStatusError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // containers/image renders errcode bodies as `<message>` /
        // `<code>: <message>` and the generic case as
        // "received unexpected HTTP status: <status>".
        if self.message.is_empty() {
            write!(f, "received unexpected HTTP status: {}", self.status)
        } else {
            write!(
                f,
                "received unexpected HTTP status: {}: {}",
                self.status, self.message
            )
        }
    }
}

impl std::error::Error for HttpStatusError {}

pub struct RegistryClient {
    agent: Agent,
    base: String,
    repository: String,
    username: Option<String>,
    password: Option<String>,
    identity_token: Option<String>,
    bearer: Option<String>,
    auth_done: bool,
    auth_failed: Option<String>,
    debug: bool,
    retries: u32,
    retry_delay: Option<Duration>,
    action: &'static str,
}

pub struct ManifestResponse {
    pub manifest: ManifestBytes,
    pub digest: Option<String>,
}

pub struct BlobDownload {
    pub reader: BodyReader<'static>,
    pub size: Option<u64>,
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    #[serde(default)]
    errors: Vec<RegistryError>,
}

#[derive(Deserialize)]
struct RegistryError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

impl RegistryClient {
    pub fn new(
        reference: &crate::reference::DockerReference,
        opts: &ImageOptions,
        action: &'static str,
    ) -> Result<Self> {
        let host = reference.api_host();
        let insecure = !opts.verify_tls();
        let scheme = if insecure && is_local_registry(&host) {
            "http"
        } else {
            "https"
        };
        let base = format!("{scheme}://{host}");

        let mut builder = Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(crate::user_agent(opts))
            .max_redirects(10);
        if let Some(timeout) = opts.timeout {
            builder = builder.timeout_global(Some(timeout));
        }
        let mut tls = TlsConfig::builder();
        if insecure {
            tls = tls.disable_verification(true);
        }
        if let Some(cert_dir) = &opts.cert_dir {
            let roots = load_cert_dir(cert_dir)?;
            if !roots.is_empty() {
                tls = tls.root_certs(RootCerts::from(roots));
            }
        }
        builder = builder.tls_config(tls.build());
        let agent: Agent = builder.build().into();

        let resolved = auth::resolve_credentials(&reference.registry, &opts.auth)?;
        let registry_token = resolved.as_ref().and_then(|c| c.registry_token.clone());
        let (username, password, identity_token) = match resolved {
            Some(creds) => (creds.username, creds.password, creds.identity_token),
            None => (None, None, None),
        };

        let mut client = RegistryClient {
            agent,
            base,
            repository: reference.repository.clone(),
            username: username.filter(|u| !u.is_empty()),
            password: password.filter(|p| !p.is_empty()),
            identity_token,
            bearer: None,
            auth_done: false,
            auth_failed: None,
            debug: opts.debug,
            retries: opts.retries,
            retry_delay: opts.retry_delay,
            action,
        };

        if let Some(token) = registry_token {
            client.bearer = Some(token);
            client.auth_done = true;
        }
        Ok(client)
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Fetch a manifest by tag or digest.
    pub fn get_manifest(&mut self, reference: &str) -> Result<ManifestResponse> {
        let url = format!("{}/v2/{}/manifests/{}", self.base, self.repository, reference);
        let resp = self.request(Method::Get, &url, &[("Accept", manifest::accepted_manifest_types())], None)?;
        let status = resp.status().as_u16();
        let content_type = header(&resp, "content-type");
        let digest = header(&resp, "docker-content-digest");
        let body = resp.into_body().read_to_vec().context("reading manifest body")?;
        if !(200..300).contains(&status) {
            return Err(self.status_error(status, &body).into());
        }
        // When fetching by digest, the returned content must hash to it.
        if let Some((algorithm, expected)) = reference.split_once(':') {
            if algorithm == "sha256" || algorithm == "sha384" || algorithm == "sha512" {
                let actual = crate::digest::digest_bytes_with(algorithm, &body)?;
                if actual != expected {
                    bail!(
                        "digest of manifest {reference} does not match the returned content (got {algorithm}:{actual})"
                    );
                }
            }
        }
        let media_type = manifest::detect_media_type(&body, content_type.as_deref());
        Ok(ManifestResponse {
            manifest: ManifestBytes {
                bytes: body,
                media_type,
            },
            digest,
        })
    }

    /// Download a blob. The caller is responsible for verifying its digest.
    pub fn get_blob(&mut self, digest: &Digest) -> Result<BlobDownload> {
        let url = format!("{}/v2/{}/blobs/{}", self.base, self.repository, digest);
        let resp = self.request(Method::Get, &url, &[("Accept", "*/*")], None)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.into_body().read_to_vec().unwrap_or_default();
            return Err(self.status_error(status, &body).into());
        }
        let size = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        Ok(BlobDownload {
            reader: resp.into_body().into_reader(),
            size,
        })
    }

    pub fn blob_exists(&mut self, digest: &Digest) -> Result<bool> {
        let url = format!("{}/v2/{}/blobs/{}", self.base, self.repository, digest);
        let resp = self.request(Method::Head, &url, &[], None)?;
        match resp.status().as_u16() {
            200..=299 => Ok(true),
            404 => Ok(false),
            status => {
                let body = resp.into_body().read_to_vec().unwrap_or_default();
                Err(self.status_error(status, &body).into())
            }
        }
    }

    /// Upload a blob unless it already exists. Returns `true` when the blob was
    /// actually uploaded.
    pub fn put_blob(
        &mut self,
        digest: &Digest,
        size: Option<u64>,
        reader: &mut dyn std::io::Read,
    ) -> Result<bool> {
        if self.blob_exists(digest)? {
            if self.debug {
                eprintln!("DEBU[0000] blob {digest} already exists on destination");
            }
            return Ok(false);
        }

        let start = format!("{}/v2/{}/blobs/uploads/", self.base, self.repository);
        let resp = self.request(Method::Post, &start, &[], Some(b""))?;
        let status = resp.status().as_u16();
        let location = header(&resp, "location");
        let body = resp.into_body().read_to_vec().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(self.status_error(status, &body).into());
        }
        if status == 201 {
            // Some registries complete a zero-byte monolithic upload here.
            return Ok(true);
        }
        let location = location.context("registry did not return an upload location")?;
        let mut upload_url = absolute_url(&self.base, &location);
        let separator = if upload_url.contains('?') { '&' } else { '?' };
        upload_url.push(separator);
        upload_url.push_str("digest=");
        upload_url.push_str(&encode_component(&digest.to_string()));

        let mut headers: Vec<(&str, String)> = vec![("Content-Type", "application/octet-stream".into())];
        if let Some(size) = size {
            headers.push(("Content-Length", size.to_string()));
        }
        let resp = self.request_reader(Method::Put, &upload_url, &headers, reader)?;
        let status = resp.status().as_u16();
        let body = resp.into_body().read_to_vec().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(self.status_error(status, &body).into());
        }
        Ok(true)
    }

    /// Push a manifest. `reference` is a tag or a digest.
    pub fn put_manifest(
        &mut self,
        reference: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<String> {
        let url = format!("{}/v2/{}/manifests/{}", self.base, self.repository, reference);
        let resp = self.request(
            Method::Put,
            &url,
            &[("Content-Type", media_type)],
            Some(bytes),
        )?;
        let status = resp.status().as_u16();
        let digest = header(&resp, "docker-content-digest");
        let body = resp.into_body().read_to_vec().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(self.status_error(status, &body).into());
        }
        Ok(digest.unwrap_or_else(|| crate::digest::digest_bytes(bytes)))
    }

    pub fn delete_manifest(&mut self, reference: &str) -> Result<()> {
        let url = format!("{}/v2/{}/manifests/{}", self.base, self.repository, reference);
        let resp = self.request(
            Method::Delete,
            &url,
            &[("Accept", manifest::accepted_manifest_types())],
            None,
        )?;
        let status = resp.status().as_u16();
        let body = resp.into_body().read_to_vec().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(self.status_error(status, &body).into());
        }
        Ok(())
    }

    /// List tags, following pagination `Link` headers.
    pub fn list_tags(&mut self) -> Result<Vec<String>> {
        let mut tags = Vec::new();
        let mut url = format!("{}/v2/{}/tags/list?n=1000", self.base, self.repository);
        let mut seen = std::collections::HashSet::new();
        loop {
            if !seen.insert(url.clone()) {
                break;
            }
            let resp = self.request(Method::Get, &url, &[("Accept", "application/json")], None)?;
            let status = resp.status().as_u16();
            let link = header(&resp, "link");
            let body = resp.into_body().read_to_vec().unwrap_or_default();
            if !(200..300).contains(&status) {
                return Err(self.status_error(status, &body).into());
            }
            let parsed: TagsResponse =
                serde_json::from_slice(&body).context("parsing tag list response")?;
            if let Some(mut list) = parsed.tags {
                tags.append(&mut list);
            }
            let _ = parsed.name;
            match link.and_then(|l| parse_next_link(&l)) {
                Some(next) => url = absolute_url(&self.base, &next),
                None => break,
            }
        }
        tags.dedup();
        Ok(tags)
    }

    /// Check authentication against `/v2/` using the configured credentials.
    /// Used by `skopeo login` to validate a username/password pair.
    pub fn check_credentials(username: &str, password: &str, registry: &str) -> Result<()> {
        let host = match registry {
            "docker.io" | "index.docker.io" => "registry-1.docker.io",
            other => other,
        };
        let base = if is_local_registry(host) {
            format!("http://{host}")
        } else {
            format!("https://{host}")
        };
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(crate::user_agent(&ImageOptions::new()))
            .build()
            .into();
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let resp = agent
            .get(format!("{base}/v2/"))
            .header("Authorization", format!("Basic {auth}"))
            .call()
            .context("contacting registry")?;
        match resp.status().as_u16() {
            200..=299 => Ok(()),
            401 | 403 => bail!("unauthorized: incorrect username or password"),
            status => {
                let body = resp.into_body().read_to_vec().unwrap_or_default();
                bail!(
                    "registry login failed: HTTP {status}{}",
                    if body.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", String::from_utf8_lossy(&body))
                    }
                )
            }
        }
    }

    fn request(
        &mut self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response<Body>> {
        let owned: Vec<(&str, String)> = headers
            .iter()
            .map(|(k, v)| (*k, (*v).to_string()))
            .collect();
        self.request_inner(method, url, &owned, body.map(BodySource::Bytes))
    }

    fn request_reader(
        &mut self,
        method: Method,
        url: &str,
        headers: &[(&str, String)],
        reader: &mut dyn std::io::Read,
    ) -> Result<Response<Body>> {
        self.ensure_auth().ok();
        let mut attempt = 0u32;
        loop {
            let mut all_headers: Vec<(String, String)> = self
                .auth_headers()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            all_headers.extend(headers.iter().map(|(k, v)| (k.to_string(), v.clone())));
            let result = match method {
                Method::Post => {
                    let mut req = self.agent.post(url);
                    for (k, v) in &all_headers {
                        req = req.header(k, v);
                    }
                    req.send(ureq::SendBody::from_reader(reader))
                }
                Method::Put => {
                    let mut req = self.agent.put(url);
                    for (k, v) in &all_headers {
                        req = req.header(k, v);
                    }
                    req.send(ureq::SendBody::from_reader(reader))
                }
                _ => bail!("request_reader only supports POST and PUT"),
            };
            match result {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    if attempt < self.retries && is_retryable_error(&err) {
                        self.sleep_backoff(attempt);
                        attempt += 1;
                        continue;
                    }
                    return Err(err).context("uploading blob");
                }
            }
        }
    }

    fn request_inner(
        &mut self,
        method: Method,
        url: &str,
        headers: &[(&str, String)],
        body: Option<BodySource<'_>>,
    ) -> Result<Response<Body>> {
        let mut reauthorized = false;
        let mut attempt = 0u32;
        loop {
            // Authenticate lazily; failures are reported by the request itself
            // so that anonymous reads can still work against open registries.
            let _ = self.ensure_auth();

            let mut all_headers: Vec<(String, String)> = self
                .auth_headers()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            all_headers.extend(headers.iter().map(|(k, v)| (k.to_string(), v.clone())));

            let result = match (method, body.as_ref()) {
                (Method::Get | Method::Head | Method::Delete, _) => {
                    let mut req = match method {
                        Method::Get => self.agent.get(url),
                        Method::Head => self.agent.head(url),
                        _ => self.agent.delete(url),
                    };
                    for (k, v) in &all_headers {
                        req = req.header(k, v);
                    }
                    req.call()
                }
                (Method::Post | Method::Put, Some(BodySource::Bytes(bytes))) => {
                    let mut req = if matches!(method, Method::Post) {
                        self.agent.post(url)
                    } else {
                        self.agent.put(url)
                    };
                    for (k, v) in &all_headers {
                        req = req.header(k, v);
                    }
                    req.send(*bytes)
                }
                (Method::Post | Method::Put, None) => {
                    let mut req = if matches!(method, Method::Post) {
                        self.agent.post(url)
                    } else {
                        self.agent.put(url)
                    };
                    for (k, v) in &all_headers {
                        req = req.header(k, v);
                    }
                    req.send(&[][..])
                }
                (_, Some(BodySource::Reader(_))) => unreachable!(),
            };
            let resp = match result {
                Ok(resp) => resp,
                Err(err) => {
                    if attempt < self.retries && is_retryable_error(&err) {
                        self.sleep_backoff(attempt);
                        attempt += 1;
                        continue;
                    }
                    return Err(err).with_context(|| format!("requesting {url}"));
                }
            };

            let status = resp.status().as_u16();
            if status == 401 && !reauthorized {
                let challenge = header(&resp, "www-authenticate");
                self.bearer = None;
                self.auth_done = false;
                if let Some(challenge) = challenge {
                    if self.authenticate_with_challenge(&challenge).is_ok() {
                        reauthorized = true;
                        continue;
                    }
                }
            }
            if (status == 429 || status >= 500) && attempt < self.retries {
                self.sleep_backoff(attempt);
                attempt += 1;
                continue;
            }
            return Ok(resp);
        }
    }


    fn auth_headers(&self) -> Vec<(&'static str, String)> {
        if let Some(token) = &self.bearer {
            return vec![("Authorization", format!("Bearer {token}"))];
        }
        if let (Some(user), Some(pass)) = (&self.username, &self.password) {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            return vec![("Authorization", format!("Basic {encoded}"))];
        }
        Vec::new()
    }

    fn ensure_auth(&mut self) -> Result<()> {
        if self.auth_done {
            return Ok(());
        }
        self.auth_done = true;

        if let Some(token) = &self.identity_token {
            self.bearer = Some(token.clone());
        }

        let ping = format!("{}/v2/", self.base);
        let mut req = self.agent.get(&ping);
        for (k, v) in self.auth_headers() {
            req = req.header(k, v);
        }
        let resp = match req.call() {
            Ok(resp) => resp,
            Err(err) => {
                self.auth_failed = Some(err.to_string());
                return Err(err).context("contacting registry");
            }
        };
        let status = resp.status().as_u16();
        let challenge = header(&resp, "www-authenticate");
        if (200..300).contains(&status) {
            return Ok(());
        }
        if status != 401 {
            let body = resp.into_body().read_to_vec().unwrap_or_default();
            let message = String::from_utf8_lossy(&body).to_string();
            self.auth_failed = Some(message.clone());
            return Err(HttpStatusError { status, message }.into());
        }
        let challenge = challenge.context("registry requires authentication but sent no challenge")?;
        self.authenticate_with_challenge(&challenge)
    }

    fn authenticate_with_challenge(&mut self, challenge: &str) -> Result<()> {
        let (scheme, params) = parse_challenge(challenge);
        if scheme.eq_ignore_ascii_case("basic") {
            if self.username.is_none() {
                bail!("registry requires basic authentication but no credentials were provided");
            }
            return Ok(());
        }
        if !scheme.eq_ignore_ascii_case("bearer") {
            bail!("unsupported authentication scheme {scheme:?}");
        }
        let realm = params
            .get("realm")
            .context("bearer challenge has no realm")?
            .clone();
        let service = params.get("service").cloned();
        let scope = params
            .get("scope")
            .cloned()
            .unwrap_or_else(|| format!("repository:{}:{}", self.repository, self.action));

        let mut url = format!(
            "{realm}{}service={}&scope={}",
            if realm.contains('?') { '&' } else { '?' },
            encode_component(service.as_deref().unwrap_or("")),
            encode_component(&scope)
        );
        if service.is_none() {
            // Harmless, but avoid sending an empty `service` parameter.
            url = format!(
                "{realm}{}scope={}",
                if realm.contains('?') { '&' } else { '?' },
                encode_component(&scope)
            );
        }
        if self.debug {
            eprintln!("DEBU[0000] fetching bearer token from {realm} (scope {scope})");
        }

        let mut req = self.agent.get(&url).header("Accept", "application/json");
        if let (Some(user), Some(pass)) = (&self.username, &self.password) {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            req = req.header("Authorization", format!("Basic {encoded}"));
        }
        let resp = req.call().context("requesting an authentication token")?;
        let status = resp.status().as_u16();
        let body = resp.into_body().read_to_vec().context("reading token response")?;
        if !(200..300).contains(&status) {
            let message = String::from_utf8_lossy(&body).to_string();
            self.auth_failed = Some(message.clone());
            return Err(HttpStatusError { status, message }.into());
        }
        let value: Value = serde_json::from_slice(&body).context("parsing token response")?;
        let token = value
            .get("token")
            .or_else(|| value.get("access_token"))
            .and_then(Value::as_str)
            .context("token response did not contain a token")?;
        self.bearer = Some(token.to_string());
        Ok(())
    }

    fn status_error(&self, status: u16, body: &[u8]) -> HttpStatusError {
        let mut message = String::new();
        if let Ok(envelope) = serde_json::from_slice::<ErrorEnvelope>(body) {
            let parts: Vec<String> = envelope
                .errors
                .iter()
                .map(|e| {
                    if e.message.is_empty() {
                        e.code.clone()
                    } else if e.code.is_empty() {
                        e.message.clone()
                    } else {
                        format!("{}: {}", e.code, e.message)
                    }
                })
                .collect();
            if !parts.is_empty() {
                message = parts.join("; ");
            }
        }
        if message.is_empty() {
            let text = String::from_utf8_lossy(body).trim().to_string();
            message = text;
        }
        if message.is_empty() && status == 401 {
            message = "unauthorized".to_string();
        }
        HttpStatusError { status, message }
    }

    fn sleep_backoff(&self, attempt: u32) {
        let delay = self
            .retry_delay
            .unwrap_or_else(|| Duration::from_secs(1u64 << attempt.min(5)));
        thread::sleep(delay);
    }
}

/// Response body source for the request helper.
enum BodySource<'a> {
    Bytes(&'a [u8]),
    #[allow(dead_code)]
    Reader(&'a mut dyn std::io::Read),
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
}

/// Load CA certificates (*.crt, *.cert) from a `--cert-dir` directory,
/// mirroring how containers/image treats registry certificate directories.
pub fn load_cert_dir(dir: &std::path::Path) -> Result<Vec<ureq::tls::Certificate<'static>>> {
    let mut certs = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!(
            "certificate directory {} does not exist",
            dir.display()
        ),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    let mut files: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("crt") | Some("cert")
            )
        })
        .collect();
    files.sort();
    for path in files {
        let data = std::fs::read(&path)
            .with_context(|| format!("reading certificate {}", path.display()))?;
        for item in ureq::tls::parse_pem(&data) {
            if let Ok(ureq::tls::PemItem::Certificate(cert)) = item {
                certs.push(cert.to_owned());
            }
        }
    }
    Ok(certs)
}

/// Validate credentials against a registry the way `skopeo login` does:
/// ping `/v2/`, follow the bearer challenge and authenticate with the
/// supplied username/password.
pub fn try_login(
    registry: &str,
    username: &str,
    password: &str,
    opts: &ImageOptions,
) -> Result<()> {
    let host = match registry {
        "docker.io" | "index.docker.io" => "registry-1.docker.io",
        other => other,
    };
    let insecure = !opts.verify_tls();
    let base = if insecure && is_local_registry(host) {
        format!("http://{host}")
    } else {
        format!("https://{host}")
    };

    let mut builder = Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(crate::user_agent(opts));
    if let Some(timeout) = opts.timeout {
        builder = builder.timeout_global(Some(timeout));
    }
    let mut tls = TlsConfig::builder();
    if insecure {
        tls = tls.disable_verification(true);
    }
    if let Some(cert_dir) = &opts.cert_dir {
        let roots = load_cert_dir(cert_dir)?;
        if !roots.is_empty() {
            tls = tls.root_certs(RootCerts::from(roots));
        }
    }
    builder = builder.tls_config(tls.build());
    let agent: Agent = builder.build().into();

    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    let resp = agent
        .get(format!("{base}/v2/"))
        .header("Authorization", format!("Basic {encoded}"))
        .call()
        .with_context(|| format!("pinging {base}/v2/"))?;
    match resp.status().as_u16() {
        200..=299 => return Ok(()),
        401 => {}
        status => {
            let body = resp.into_body().read_to_vec().unwrap_or_default();
            return Err(HttpStatusError { status, message: String::from_utf8_lossy(&body).to_string() }.into());
        }
    }

    // 401: follow the challenge, authenticating against the token realm.
    let challenge = match resp.headers().get("www-authenticate") {
        Some(value) => value.to_str().unwrap_or("").to_string(),
        None => bail!("registry requires authentication but sent no challenge"),
    };
    let (scheme, params) = parse_challenge(&challenge);
    if scheme.eq_ignore_ascii_case("basic") {
        bail!("unauthorized: incorrect username or password");
    }
    if !scheme.eq_ignore_ascii_case("bearer") {
        bail!("unsupported authentication scheme {scheme:?}");
    }
    let realm = params
        .get("realm")
        .with_context(|| format!("bearer challenge from {base} has no realm"))?;
    let mut url = realm.clone();
    let separator = if realm.contains('?') { '&' } else { '?' };
    url.push(separator);
    if let Some(service) = params.get("service") {
        url.push_str("service=");
        url.push_str(&encode_component(service));
        url.push('&');
    }
    if let Some(scope) = params.get("scope") {
        url.push_str("scope=");
        url.push_str(&encode_component(scope));
        url.push('&');
    }
    url.push_str("account=");
    url.push_str(&encode_component(username));

    let resp = agent
        .get(&url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Basic {encoded}"))
        .call()
        .context("requesting an authentication token")?;
    let status = resp.status().as_u16();
    let body = resp.into_body().read_to_vec().unwrap_or_default();
    match status {
        200..=299 => Ok(()),
        401 | 403 => bail!("logging into {registry:?}: invalid username/password"),
        status => Err(HttpStatusError {
            status,
            message: String::from_utf8_lossy(&body).to_string(),
        }
        .into()),
    }
}

fn header(resp: &Response<Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn is_retryable_error(err: &ureq::Error) -> bool {
    matches!(err, ureq::Error::Timeout(_) | ureq::Error::Io(_) | ureq::Error::ConnectionFailed)
}

fn is_local_registry(host: &str) -> bool {
    let host = host.split(':').next().unwrap_or(host);
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

fn absolute_url(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        location.to_string()
    } else if location.starts_with('/') {
        format!("{base}{location}")
    } else {
        format!("{base}/{location}")
    }
}

/// Parse `Bearer realm="...",service="..."` / `Basic realm="..."`.
fn parse_challenge(challenge: &str) -> (String, HashMap<String, String>) {
    let challenge = challenge.trim();
    let (scheme, rest) = match challenge.split_once(char::is_whitespace) {
        Some((scheme, rest)) => (scheme.to_string(), rest.trim()),
        None => (challenge.to_string(), ""),
    };
    let mut params = HashMap::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut tokens = Vec::new();
    for c in rest.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            ',' if !in_quotes => {
                tokens.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        tokens.push(current.trim().to_string());
    }
    for token in tokens {
        if let Some((key, value)) = token.split_once('=') {
            let value = value.trim().trim_matches('"').to_string();
            params.insert(key.trim().to_ascii_lowercase(), value);
        }
    }
    (scheme, params)
}

fn parse_next_link(link: &str) -> Option<String> {
    for part in link.split(',') {
        let part = part.trim();
        if !part.contains("rel=\"next\"") && !part.contains("rel=next") {
            continue;
        }
        let start = part.find('<')?;
        let end = part.find('>')?;
        if end > start {
            return Some(part[start + 1..end].to_string());
        }
    }
    None
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Compute the digest of a manifest the way the registry does.
pub fn manifest_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
}

/// Human readable progress helper used by the copy engine.
pub fn describe_blob(digest: &Digest, size: Option<u64>) -> String {
    match size {
        Some(size) => format!("{digest} ({})", util::human_size(size)),
        None => digest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_challenges() {
        let (scheme, params) = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#,
        );
        assert_eq!(scheme, "Bearer");
        assert_eq!(params["realm"], "https://auth.docker.io/token");
        assert_eq!(params["service"], "registry.docker.io");
        assert_eq!(params["scope"], "repository:library/alpine:pull");
    }

    #[test]
    fn parses_basic_challenges() {
        let (scheme, params) = parse_challenge(r#"Basic realm="Registry Realm""#);
        assert_eq!(scheme, "Basic");
        assert_eq!(params["realm"], "Registry Realm");
    }

    #[test]
    fn parses_pagination_links() {
        assert_eq!(
            parse_next_link(r#"</v2/_catalog?last=foo&n=100>; rel="next""#).unwrap(),
            "/v2/_catalog?last=foo&n=100"
        );
        assert!(parse_next_link(r#"</v2/x>; rel="prev""#).is_none());
    }

    #[test]
    fn url_encoding() {
        assert_eq!(encode_component("a:b/c"), "a%3Ab%2Fc");
    }
}
