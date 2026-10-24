//! `skopeo standalone-sign`, `standalone-verify` and
//! `untrusted-signature-dump-without-verification`.
//!
//! The upstream implementation links gpgme. This port shells out to the
//! `gpg` binary instead, keeping the static binary small; the produced and
//! consumed signature blobs are the same simple-signing format
//! (a GPG signed message whose payload is the `critical`/`optional` JSON).

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::ArgMatches;

use crate::commands::write_stdout;
use crate::util;
use crate::copy::manifest_digest_from_bytes;
use crate::reference::DockerReference;

/// Build the simple-signing payload JSON with the exact field order the Go
/// implementation produces (map keys are marshalled sorted).
fn signature_payload(digest: &str, reference: &str, creator: Option<&str>, timestamp: Option<i64>) -> Vec<u8> {
    let mut optional = serde_json::Map::new();
    if let Some(creator) = creator {
        optional.insert("creator".to_string(), serde_json::Value::String(creator.to_string()));
    }
    if let Some(timestamp) = timestamp {
        optional.insert("timestamp".to_string(), serde_json::Value::from(timestamp));
    }
    let payload = serde_json::json!({
        "critical": {
            "identity": { "docker-reference": reference },
            "image": { "docker-manifest-digest": digest },
            "type": "atomic container signature",
        },
        "optional": optional,
    });
    serde_json::to_vec(&payload).expect("payload serializes")
}

pub fn standalone_sign(m: &ArgMatches) -> Result<i32> {
    let manifest_path = m.get_one::<String>("MANIFEST").expect("required");
    let docker_reference = m.get_one::<String>("DOCKER-REFERENCE").expect("required");
    let fingerprint = m.get_one::<String>("KEY-FINGERPRINT").expect("required");
    let output = m.get_one::<String>("output").expect("required");
    let passphrase_file = m.get_one::<String>("passphrase-file").filter(|v| !v.is_empty());

    let manifest_bytes = std::fs::read(manifest_path)
        .with_context(|| format!("reading manifest {manifest_path:?}"))?;
    let digest = manifest_digest_from_bytes(&manifest_bytes)?;
    // Go embeds the docker reference string as given, without normalizing.
    let _ = DockerReference::parse(docker_reference)
        .with_context(|| format!("invalid docker reference {docker_reference:?}"))?;

    let payload = signature_payload(
        &digest,
        docker_reference,
        Some(&format!("atomic {}", crate::VERSION)),
        Some(unix_now()),
    );

    let temp = crate::util::temp_dir(None, "skopeo-sign")?;
    let payload_path = temp.path().join("payload.json");
    std::fs::write(&payload_path, &payload)
        .with_context(|| format!("writing {}", payload_path.display()))?;

    let mut command = Command::new("gpg");
    command
        .arg("--batch")
        .arg("--yes")
        .arg("--pinentry-mode")
        .arg("loopback")
        .arg("--armor")
        .arg("--local-user")
        .arg(fingerprint)
        .arg("--output")
        .arg(output);
    if let Some(passphrase_file) = passphrase_file {
        command.arg("--passphrase-file").arg(passphrase_file);
    }
    command.arg("--sign").arg(&payload_path);

    let status = command
        .status()
        .map_err(|err| anyhow::anyhow!("running gpg (is it installed?): {err}"))?;
    if !status.success() {
        bail!("gpg signing failed");
    }
    crate::util::set_public_permissions(Path::new(output))?;
    Ok(0)
}

pub fn standalone_verify(m: &ArgMatches) -> Result<i32> {
    let manifest_path = m.get_one::<String>("MANIFEST").expect("required");
    let docker_reference = m.get_one::<String>("DOCKER-REFERENCE").expect("required");
    let fingerprints_arg = m.get_one::<String>("KEY-FINGERPRINTS").expect("required");
    let signature_path = m.get_one::<String>("SIGNATURE").expect("required");
    let public_key_file = m
        .get_one::<String>("public-key-file")
        .filter(|v| !v.is_empty());

    let fingerprints: Vec<String> = fingerprints_arg
        .split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect();
    if fingerprints.is_empty() {
        bail!("no key fingerprints specified");
    }
    let accept_any = fingerprints.len() == 1 && fingerprints[0] == "any";
    if accept_any && public_key_file.is_none() {
        bail!("cannot use any fingerprint without a public key file");
    }

    let manifest_bytes = std::fs::read(manifest_path)
        .with_context(|| format!("reading manifest {manifest_path:?}"))?;
    let expected_digest = manifest_digest_from_bytes(&manifest_bytes)?;
    let reference = DockerReference::parse(docker_reference)
        .with_context(|| format!("parsing docker reference {docker_reference:?}"))?;
    let expected_reference = reference.familiar_name();

    let temp = crate::util::temp_dir(None, "skopeo-verify")?;
    let payload_path = temp.path().join("payload.json");

    let mut command = Command::new("gpg");
    command.arg("--batch");
    if let Some(public_key_file) = public_key_file {
        let home = temp.path().join("gnupg");
        std::fs::create_dir_all(&home)?;
        crate::util::set_private_permissions(&home)?;
        command.arg("--homedir").arg(&home);
        let status = Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .arg("--batch")
            .arg("--import")
            .arg(public_key_file)
            .status()
            .map_err(|err| anyhow::anyhow!("running gpg (is it installed?): {err}"))?;
        if !status.success() {
            bail!("importing public keys from {public_key_file:?} failed");
        }
    }
    let status = command
        .arg("--status-fd")
        .arg("1")
        .arg("--output")
        .arg(&payload_path)
        .arg("--decrypt")
        .arg(signature_path)
        .output()
        .map_err(|err| anyhow::anyhow!("running gpg (is it installed?): {err}"))?;

    let stdout = String::from_utf8_lossy(&status.stdout);
    let mut fingerprint: Option<String> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("[GNUPG:] VALIDSIG ") {
            // VALIDSIG <fpr> <creation> <sig-timestamp> ...
            if let Some(fpr) = rest.split_whitespace().next() {
                fingerprint = Some(fpr.to_string());
            }
        }
    }
    let Some(fingerprint) = fingerprint else {
        bail!(
            "signature verification failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        );
    };
    if !accept_any
        && !fingerprints
            .iter()
            .any(|f| f.eq_ignore_ascii_case(&fingerprint))
    {
        bail!("signature by key {fingerprint} is not accepted");
    }

    let payload_bytes = std::fs::read(&payload_path)
        .with_context(|| format!("reading extracted signature payload {}", payload_path.display()))?;
    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes)
        .context("parsing signature payload")?;
    let signed_digest = payload
        .pointer("/critical/image/docker-manifest-digest")
        .and_then(|v| v.as_str())
        .context("signature payload has no docker-manifest-digest")?;
    let signed_reference = payload
        .pointer("/critical/identity/docker-reference")
        .and_then(|v| v.as_str())
        .context("signature payload has no docker-reference")?;
    if signed_digest != expected_digest {
        bail!("verifying signature: Signature for docker digest {signed_digest:?} does not match");
    }
    if signed_reference != expected_reference {
        bail!(
            "verifying signature: Docker reference {signed_reference:?} does not match {expected_reference:?}"
        );
    }
    // gpgme reports uppercase fingerprints.
    let fingerprint = fingerprint.to_uppercase();
    println!("Signature verified using fingerprint {fingerprint}, digest {signed_digest}");
    Ok(0)
}

pub fn untrusted_dump(m: &ArgMatches) -> Result<i32> {
    let signature_path = m.get_one::<String>("SIGNATURE").expect("required");
    let bytes = std::fs::read(signature_path)
        .with_context(|| format!("reading signature {signature_path:?}"))?;

    let payload = extract_signed_payload(&bytes)?;
    let payload: serde_json::Value =
        serde_json::from_slice(&payload).context("parsing signature payload")?;
    let digest = payload
        .pointer("/critical/image/docker-manifest-digest")
        .and_then(|v| v.as_str())
        .context("signature payload has no docker-manifest-digest")?;
    let reference = payload
        .pointer("/critical/identity/docker-reference")
        .and_then(|v| v.as_str())
        .context("signature payload has no docker-reference")?;
    let creator = payload
        .pointer("/optional/creator")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let timestamp = payload
        .pointer("/optional/timestamp")
        .and_then(|v| v.as_i64());

    let report = serde_json::json!({
        "UntrustedDockerManifestDigest": digest,
        "UntrustedDockerReference": reference,
        "UntrustedCreatorID": creator,
        "UntrustedTimestamp": timestamp.map(util::rfc3339_from_unix),
        "UntrustedShortKeyIdentifier": short_key_identifier(&bytes).to_uppercase(),
    });
    write_stdout(util::go_json_string(&report)?.as_bytes())?;
    Ok(0)
}

/// Extract the signed JSON from a GPG signed message or clear-signed blob.
fn extract_signed_payload(bytes: &[u8]) -> Result<Vec<u8>> {
    let text = String::from_utf8_lossy(bytes);
    if text.contains("-----BEGIN PGP SIGNED MESSAGE-----") {
        // Clear-signed: the payload is the dash-escaped body.
        let mut body = Vec::new();
        let mut in_body = false;
        for line in text.lines() {
            if line.starts_with("-----BEGIN PGP SIGNED MESSAGE-----") {
                in_body = true;
                continue;
            }
            if !in_body {
                continue;
            }
            if line.starts_with("-----BEGIN PGP SIGNATURE-----") {
                return Ok(body);
            }
            if body.is_empty() && (line.starts_with("Hash:") || line.trim().is_empty()) {
                continue;
            }
            let unescaped = line.strip_prefix("- ").unwrap_or(line);
            body.extend_from_slice(unescaped.as_bytes());
            body.push(b'\n');
        }
        if body.is_empty() {
            bail!("malformed clear-signed message");
        }
        return Ok(body);
    }
    if text.contains("-----BEGIN PGP MESSAGE-----") {
        // Attached signed message: let gpg extract the literal data packet.
        let temp = crate::util::temp_dir(None, "skopeo-dump")?;
        let in_path = temp.path().join("sig");
        std::fs::write(&in_path, bytes).context("writing signature to temporary file")?;
        let out_path = temp.path().join("payload");
        let status = Command::new("gpg")
            .arg("--batch")
            .arg("--output")
            .arg(&out_path)
            .arg("--decrypt")
            .arg(&in_path)
            .output()
            .map_err(|err| anyhow::anyhow!("running gpg (is it installed?): {err}"))?;
        let _ = status;
        let data = std::fs::read(&out_path)
            .with_context(|| "extracting signed payload with gpg".to_string())?;
        return Ok(data);
    }
    // Maybe the file is the raw payload JSON already.
    if serde_json::from_slice::<serde_json::Value>(bytes)
        .map(|v| v.get("critical").is_some())
        .unwrap_or(false)
    {
        return Ok(bytes.to_vec());
    }
    bail!("unrecognized signature format")
}

/// Best-effort short key identifier via `gpg --list-packets`.
fn short_key_identifier(bytes: &[u8]) -> String {
    let inner = || -> Option<String> {
        let temp = crate::util::temp_dir(None, "skopeo-dump").ok()?;
        let path = temp.path().join("sig");
        std::fs::write(&path, bytes).ok()?;
        let output = Command::new("gpg")
            .arg("--batch")
            .arg("--list-packets")
            .arg(&path)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix(":signature packet:") {
                let mut tokens = rest.split_whitespace();
                while let Some(field) = tokens.next() {
                    if field.starts_with("keyid") {
                        if let Some(keyid) = field.strip_prefix("keyid=") {
                            return Some(keyid.trim_start_matches(':').to_string());
                        }
                        if let Some(keyid) = tokens.next() {
                            return Some(keyid.trim_start_matches(':').to_string());
                        }
                    }
                }
            }
        }
        None
    };
    inner().unwrap_or_default()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

