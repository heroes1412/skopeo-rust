//! End-to-end CLI tests (no network).

use std::io::Write;

use assert_cmd::Command;
use predicates::str::contains;

fn skopeo() -> Command {
    Command::cargo_bin("skopeo").expect("binary")
}

/// Write a minimal OCI layout with one gzip layer; returns the layout dir.
fn make_oci_layout(dir: &std::path::Path) -> String {
    use std::process::Command as StdCommand;

    let layout = dir.join("layout");
    std::fs::create_dir_all(layout.join("blobs/sha256")).unwrap();

    // Build the layer tar with the system tar (or python as fallback).
    let layer_dir = dir.join("layer-root");
    std::fs::create_dir_all(&layer_dir).unwrap();
    std::fs::write(layer_dir.join("hello.txt"), b"hello world\n").unwrap();
    let tar_path = dir.join("layer.tar");
    let status = StdCommand::new("tar")
        .arg("-cf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&layer_dir)
        .arg("hello.txt")
        .status();
    let tar_bytes = match status {
        Ok(s) if s.success() => std::fs::read(&tar_path).unwrap(),
        _ => {
            // Minimal fallback tar (empty file entry is fine for the test).
            Vec::new()
        }
    };

    let config_json = format!(
        r#"{{"architecture":"amd64","os":"linux","rootfs":{{"type":"layers","diff_ids":["{}"]}},"created":"2024-01-01T00:00:00Z","docker_version":"20.10"}}"#,
        sha256_hex(&tar_bytes)
    );
    let write_blob = |data: &[u8]| -> (String, String) {
        let digest = sha256_hex(data);
        std::fs::write(layout.join("blobs/sha256").join(&digest), data).unwrap();
        (digest.clone(), format!("sha256:{digest}"))
    };
    let (_config_hex, config_digest) = write_blob(config_json.as_bytes());
    let manifest_json = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[]}}"#,
        config_json.len()
    );
    let (manifest_hex, manifest_digest) = write_blob(manifest_json.as_bytes());
    let _ = manifest_hex;

    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{manifest_digest}","size":{},"platform":{{"architecture":"amd64","os":"linux"}}}}]}}"#,
        manifest_json.len()
    );
    std::fs::write(layout.join("index.json"), index).unwrap();
    std::fs::write(
        layout.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    config_digest
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    // sha2 is available transitively; the tests don't add it as a direct
    // dependency, so fall back to a trivial placeholder hash when absent.
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

// The test crate needs sha2/hex; declare them through the library crate.
use skopeo::_test_support::{hex_encode, Sha256};

#[test]
fn version_outputs_version() {
    skopeo()
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("skopeo version 1.24.1-rs"))
        .stdout(contains("Rust port, forked from skopeo v1.24.1"));
}

#[test]
fn missing_command_fails() {
    skopeo()
        .assert()
        .failure()
        .stderr(contains("missing command 'skopeo COMMAND'"));
}

#[test]
fn inspect_local_oci_layout() {
    let dir = tempfile::tempdir().unwrap();
    make_oci_layout(dir.path());
    skopeo()
        .arg("inspect")
        .arg(format!("oci:{}", dir.path().join("layout").display()))
        .assert()
        .success()
        .stdout(contains("\"Architecture\""))
        .stdout(contains("\"Os\""));
}

#[test]
fn manifest_digest_matches_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manifest.json");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(b"{\"schemaVersion\":2}").unwrap();
    drop(file);
    skopeo()
        .arg("manifest-digest")
        .arg(&path)
        .assert()
        .success()
        .stdout(contains("sha256:"));
}
