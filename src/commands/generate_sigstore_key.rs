//! `skopeo generate-sigstore-key`.
//!
//! The upstream command writes a sigstore (cosign-style) encrypted ECDSA
//! key pair. Reproducing the exact encrypted key format would drag in a
//! full crypto stack, so this port reports the operation as unsupported
//! while keeping the CLI surface identical.

use anyhow::{bail, Context, Result};
use clap::ArgMatches;

pub fn run(m: &ArgMatches) -> Result<i32> {
    if m.get_many::<String>("ARGS").map(|v| v.count()).unwrap_or(0) != 0 {
        bail!("no arguments expected");
    }
    let prefix = m
        .get_one::<String>("output-prefix")
        .filter(|v| !v.is_empty())
        .context("--output-prefix must be specified")?;
    if m.get_one::<String>("output-prefix").is_none() {
        bail!("--output-prefix must be specified");
    }
    let _ = prefix;
    bail!(
        "generate-sigstore-key is not supported by this Rust port: producing the \
         cosign encrypted-key format requires a PGP/sigstore crypto stack that \
         was intentionally left out to keep the binary small. Use `cosign \
         generate-key-pair` to create an equivalent key pair."
    )
}
