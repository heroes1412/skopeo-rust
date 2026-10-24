//! Registry credentials: `auth.json` / Docker `config.json` handling,
//! credential helpers, and login/logout storage.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::{Map, Value};

use crate::options::AuthOptions;
use crate::util;

#[derive(Clone, Debug, Default)]
pub struct AuthCreds {
    pub username: Option<String>,
    pub password: Option<String>,
    pub identity_token: Option<String>,
    /// Token to use directly as `Authorization: Bearer`.
    pub registry_token: Option<String>,
}

impl AuthCreds {
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        AuthCreds {
            username: Some(username.into()),
            password: Some(password.into()),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.registry_token.is_none()
            && self.identity_token.is_none()
            && self.username.is_none()
            && self.password.is_none()
    }
}

/// The on-disk credential file.
#[derive(Debug)]
pub struct AuthFile {
    path: PathBuf,
    doc: Value,
}

impl AuthFile {
    /// Default write location: `$REGISTRY_AUTH_FILE`, then
    /// `$XDG_RUNTIME_DIR/containers/auth.json`, then
    /// `~/.config/containers/auth.json`.
    pub fn default_path() -> PathBuf {
        if let Some(path) = util::env_non_empty("REGISTRY_AUTH_FILE") {
            return PathBuf::from(path);
        }
        if let Some(runtime) = util::runtime_dir() {
            let path = runtime.join("containers/auth.json");
            if path.exists() {
                return path;
            }
        }
        if let Some(config) = util::config_home() {
            let containers = config.join("containers/auth.json");
            if containers.exists() {
                return containers;
            }
            if let Some(docker_config) = docker_config_path() {
                if docker_config.exists() {
                    return docker_config;
                }
            }
            return containers;
        }
        PathBuf::from("auth.json")
    }

    pub fn resolve_path(explicit: Option<&Path>) -> PathBuf {
        match explicit {
            Some(path) => path.to_path_buf(),
            None => Self::default_path(),
        }
    }

    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = Self::resolve_path(explicit);
        let doc = if path.exists() {
            let data = std::fs::read(&path)
                .with_context(|| format!("reading auth file {}", path.display()))?;
            if data.iter().all(u8::is_ascii_whitespace) {
                Value::Object(Map::new())
            } else {
                serde_json::from_slice(&data)
                    .with_context(|| format!("parsing auth file {}", path.display()))?
            }
        } else {
            Value::Object(Map::new())
        };
        Ok(AuthFile { path, doc })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Raw access to the underlying JSON document (for `logout --all`).
    pub fn doc_mut(&mut self) -> &mut Value {
        &mut self.doc
    }

    fn auths(&self) -> Option<&Map<String, Value>> {
        self.doc.get("auths").and_then(Value::as_object)
    }

    fn auths_mut(&mut self) -> &mut Map<String, Value> {
        if !self.doc.is_object() {
            self.doc = Value::Object(Map::new());
        }
        let object = self.doc.as_object_mut().expect("object");
        object
            .entry("auths")
            .or_insert_with(|| Value::Object(Map::new()));
        object
            .get_mut("auths")
            .and_then(Value::as_object_mut)
            .expect("auths object")
    }

    /// Find an entry for `registry`, tolerating the usual `https://…/v1/`
    /// spellings written by Docker and Podman. Repository-scoped keys are
    /// walked from the longest namespace down to the host, matching Go's
    /// `authKeyLookupOrder`.
    pub fn lookup(&self, registry: &str) -> Option<StoredAuth> {
        for key in lookup_keys(registry) {
            if let Some(stored) = self.lookup_exact(&key) {
                return Some(stored);
            }
        }
        None
    }

    fn lookup_exact(&self, key: &str) -> Option<StoredAuth> {
        let auths = self.auths()?;
        for candidate in registry_keys(key) {
            if let Some(entry) = auths.get(&candidate) {
                if let Some(stored) = parse_entry(entry) {
                    return Some(stored);
                }
            }
        }
        None
    }

    pub fn credential_helper(&self, registry: &str) -> Option<String> {
        let object = self.doc.as_object()?;
        let helpers = object.get("credHelpers").and_then(Value::as_object)?;
        for key in lookup_keys(registry) {
            for candidate in registry_keys(&key) {
                if let Some(helper) = helpers.get(&candidate).and_then(Value::as_str) {
                    return Some(helper.to_string());
                }
            }
        }
        None
    }

    pub fn set(&mut self, registry: &str, username: &str, password: &str, identity_token: Option<&str>) {
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{username}:{password}"));
        let mut entry = Map::new();
        entry.insert("auth".to_string(), Value::String(auth));
        if let Some(token) = identity_token {
            entry.insert("identitytoken".to_string(), Value::String(token.to_string()));
        }
        let auths = self.auths_mut();
        // Remove alternative spellings so lookup is unambiguous.
        for key in registry_keys(registry) {
            auths.remove(&key);
        }
        auths.insert(registry.to_string(), Value::Object(entry));
    }

    pub fn remove(&mut self, registry: &str) -> bool {
        let auths = self.auths_mut();
        let mut removed = false;
        for key in registry_keys(registry) {
            if auths.remove(&key).is_some() {
                removed = true;
            }
        }
        removed
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let pretty = serde_json::to_string_pretty(&self.doc)?;
        // Go's authfile writer uses json.MarshalIndent(..., "", "\t") and no
        // trailing newline.
        let tabbed: Vec<String> = pretty
            .lines()
            .map(|line| {
                let indent = (line.len() - line.trim_start().len()) / 2;
                let mut out = String::new();
                for _ in 0..indent {
                    out.push('\t');
                }
                out.push_str(line.trim_start());
                out
            })
            .collect();
        util::atomic_write(&self.path, tabbed.join("\n").as_bytes())?;
        util::set_private_permissions(&self.path)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct StoredAuth {
    pub username: String,
    pub password: String,
    pub identity_token: Option<String>,
}

fn parse_entry(entry: &Value) -> Option<StoredAuth> {
    let object = entry.as_object()?;
    let identity_token = object
        .get("identitytoken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(auth) = object.get("auth").and_then(Value::as_str) {
        if !auth.is_empty() {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(auth)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())?;
            let (username, password) = decoded.split_once(':').unwrap_or((decoded.as_str(), ""));
            return Some(StoredAuth {
                username: username.to_string(),
                password: password.to_string(),
                identity_token,
            });
        }
    }
    if let (Some(username), Some(password)) = (
        object.get("username").and_then(Value::as_str),
        object.get("password").and_then(Value::as_str),
    ) {
        return Some(StoredAuth {
            username: username.to_string(),
            password: password.to_string(),
            identity_token,
        });
    }
    None
}

/// All spellings used by Docker/Podman auth files for a registry.
fn registry_keys(registry: &str) -> Vec<String> {
    let registry = registry
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .trim_end_matches("/v2")
        .to_string();
    let mut keys = vec![
        registry.clone(),
        format!("https://{registry}"),
        format!("https://{registry}/v1/"),
        format!("https://{registry}/v2/"),
        format!("{registry}/v1/"),
    ];
    if matches!(registry.as_str(), "docker.io" | "index.docker.io" | "registry-1.docker.io") {
        for key in [
            "docker.io",
            "index.docker.io",
            "registry-1.docker.io",
            "https://index.docker.io/v1/",
            "https://index.docker.io/v1",
            "https://registry-1.docker.io/v1/",
            "https://docker.io/v1/",
        ] {
            keys.push(key.to_string());
        }
    }
    let mut seen = HashSet::new();
    keys.retain(|key| seen.insert(key.clone()));
    keys
}

pub fn docker_config_path() -> Option<PathBuf> {
    if let Some(dir) = util::env_non_empty("DOCKER_CONFIG") {
        return Some(PathBuf::from(dir).join("config.json"));
    }
    util::home_dir().map(|home| home.join(".docker/config.json"))
}

/// Resolve credentials from flags, the auth file, or a credential helper.
pub fn resolve_credentials(registry: &str, opts: &AuthOptions) -> Result<Option<AuthCreds>> {
    let opts = opts.normalized()?;
    if opts.no_creds {
        return Ok(None);
    }
    if let Some(token) = &opts.registry_token {
        return Ok(Some(AuthCreds {
            registry_token: Some(token.clone()),
            ..Default::default()
        }));
    }
    if let Some(creds) = &opts.creds {
        return Ok(Some(AuthCreds::basic(
            creds.username.clone(),
            creds.password.clone(),
        )));
    }

    let auth_file = AuthFile::load(opts.authfile.as_deref())?;
    if let Some(helper) = auth_file.credential_helper(registry) {
        if let Some(creds) = run_credential_helper(&helper, registry)? {
            return Ok(Some(creds));
        }
    }
    if let Some(stored) = auth_file.lookup(registry) {
        if stored.password.is_empty() && stored.identity_token.is_none() && stored.username.is_empty()
        {
            return Ok(None);
        }
        return Ok(Some(AuthCreds {
            username: Some(stored.username),
            password: Some(stored.password),
            identity_token: stored.identity_token,
            registry_token: None,
        }));
    }
    Ok(None)
}

/// Keys to try for `registry`, walking repository namespaces from the
/// longest (`quay.io/org/repo` -> `quay.io/org` -> `quay.io`).
fn lookup_keys(registry: &str) -> Vec<String> {
    let mut keys = vec![registry.to_string()];
    let mut path = registry
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    while let Some((parent, _)) = path.rsplit_once('/') {
        if parent.is_empty() {
            break;
        }
        keys.push(parent.to_string());
        path = parent;
    }
    keys
}

fn run_credential_helper(helper: &str, registry: &str) -> Result<Option<AuthCreds>> {
    let program = if helper.starts_with("docker-credential-") {
        helper.to_string()
    } else {
        format!("docker-credential-{helper}")
    };
    let mut child = match Command::new(&program)
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            // Missing helpers are not fatal: fall back to anonymous access.
            eprintln!("warning: credential helper {program:?} unavailable: {err}");
            return Ok(None);
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(registry.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let username = value
        .get("Username")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let secret = value
        .get("Secret")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if username.is_empty() && secret.is_empty() {
        return Ok(None);
    }
    if username == "<token>" {
        return Ok(Some(AuthCreds {
            username: None,
            password: None,
            identity_token: Some(secret),
            registry_token: None,
        }));
    }
    Ok(Some(AuthCreds::basic(username, secret)))
}

/// Validate that `registry` looks like a registry host and strip schemes and
/// paths the way `docker login` accepts them.
pub fn normalize_registry(input: &str) -> Result<String> {
    let input = input.trim();
    let without_scheme = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .unwrap_or(input);
    let host = without_scheme.split('/').next().unwrap_or("");
    if host.is_empty() {
        bail!("invalid registry name {input:?}");
    }
    if host.contains(char::is_whitespace) {
        bail!("invalid registry name {input:?}");
    }
    Ok(host.to_string())
}

/// Normalize a login/logout argument: strip the scheme, keep the
/// registry[/namespace[/repository]] path, and validate it.
pub fn parse_credentials_key(input: &str) -> Result<String> {
    let input = input.trim();
    let without_scheme = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .unwrap_or(input)
        .trim_end_matches('/');
    if without_scheme.is_empty() || without_scheme.contains(char::is_whitespace) {
        bail!("invalid registry name {input:?}");
    }
    Ok(without_scheme.to_string())
}

/// Get the stored username, for `login --get-login`.
pub fn get_login(registry: &str, opts: &AuthOptions) -> Result<String> {
    let auth_file = AuthFile::load(opts.authfile.as_deref())?;
    match auth_file.lookup(registry) {
        Some(stored) if !stored.username.is_empty() => Ok(stored.username),
        _ => bail!("not logged into {registry}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_authfile() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        (dir, path)
    }

    #[test]
    fn roundtrip_auth_file() {
        let (_dir, path) = temp_authfile();
        let mut file = AuthFile::load(Some(&path)).unwrap();
        file.set("quay.io", "user", "pa:ss", None);
        file.save().unwrap();

        let file = AuthFile::load(Some(&path)).unwrap();
        let stored = file.lookup("quay.io").unwrap();
        assert_eq!(stored.username, "user");
        assert_eq!(stored.password, "pa:ss");

        let mut file = AuthFile::load(Some(&path)).unwrap();
        assert!(file.remove("quay.io"));
        assert!(file.lookup("quay.io").is_none());
    }

    #[test]
    fn docker_hub_spellings() {
        let (_dir, path) = temp_authfile();
        let mut file = AuthFile::load(Some(&path)).unwrap();
        file.set("docker.io", "hubuser", "hubpass", None);
        file.save().unwrap();

        let file = AuthFile::load(Some(&path)).unwrap();
        assert_eq!(
            file.lookup("registry-1.docker.io").unwrap().username,
            "hubuser"
        );
        assert_eq!(file.lookup("docker.io").unwrap().username, "hubuser");
    }

    #[test]
    fn normalizes_registry_names() {
        assert_eq!(normalize_registry("https://quay.io").unwrap(), "quay.io");
        assert_eq!(normalize_registry("quay.io/org/repo").unwrap(), "quay.io");
        assert!(normalize_registry("").is_err());
    }
}
