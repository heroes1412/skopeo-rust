//! skopeo — a Rust port (Rust implementation) of [skopeo](https://github.com/podman-container-tools/skopeo).
//!
//! The crate is split into the same conceptual layers that the original Go
//! implementation uses:
//!
//! * [`reference`] — parsing of `transport:details` image names.
//! * [`manifest`]  — OCI / Docker manifest documents and platform selection.
//! * [`registry`]  — Docker Registry HTTP API v2 client (auth, manifests, blobs).
//! * [`transports`] — `docker://`, `dir:`, `oci:`, `oci-archive:`, `docker-archive:`.
//! * [`copy`]      — the transport-agnostic image copy engine.
//! * [`commands`]  — one module per CLI subcommand.

pub mod archive;
pub mod auth;
pub mod cli;
pub mod commands;
pub mod copy;
pub mod digest;
pub mod manifest;
pub mod options;
pub mod reference;
pub mod registry;
pub mod transports;
pub mod util;

pub use anyhow::Result;

/// Helpers for the integration test crate (`doc(hidden)`, not public API).
#[doc(hidden)]
pub mod _test_support {
    pub use sha2::Sha256;

    pub fn hex_encode(bytes: &[u8]) -> String {
        hex::encode(bytes)
    }
}

/// The upstream skopeo release this Rust port forked from.
pub const UPSTREAM_VERSION: &str = "v1.24.1";

/// User agent sent to registries, mirroring `skopeo/<version>`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn user_agent(opts: &options::ImageOptions) -> String {
    match &opts.user_agent_prefix {
        Some(prefix) if !prefix.is_empty() => format!("{prefix} skopeo/{VERSION}"),
        _ => format!("skopeo/{VERSION}"),
    }
}
