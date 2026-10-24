//! One module per CLI subcommand, mirroring `cmd/skopeo/*.go`.

pub mod completions;
pub mod copy;
pub mod delete;
pub mod generate_sigstore_key;
pub mod inspect;
pub mod layers;
pub mod list_tags;
pub mod login;
pub mod logout;
pub mod manifest_digest;
pub mod signing;
pub mod sync;

use anyhow::{bail, Context, Result};
use clap::ArgMatches;

use crate::cli::GlobalOptions;

/// Run a subcommand. Returns the process exit code.
pub fn dispatch(name: &str, matches: &ArgMatches, global: &GlobalOptions) -> Result<i32> {
    match name {
        "copy" => copy::run(matches, global).map(|_| 0),
        "inspect" => inspect::run(matches, global).map(|_| 0),
        "list-tags" => list_tags::run(matches, global).map(|_| 0),
        "delete" => delete::run(matches, global).map(|_| 0),
        "login" => login::run(matches, global).map(|_| 0),
        "logout" => logout::run(matches, global).map(|_| 0),
        "sync" => sync::run(matches, global).map(|_| 0),
        "manifest-digest" => manifest_digest::run(matches).map(|_| 0),
        "standalone-sign" => signing::standalone_sign(matches),
        "standalone-verify" => signing::standalone_verify(matches),
        "untrusted-signature-dump-without-verification" => {
            signing::untrusted_dump(matches).map(|_| 0)
        }
        "generate-sigstore-key" => generate_sigstore_key::run(matches),
        "layers" => layers::run(matches, global).map(|_| 0),
        "experimental-image-proxy" => {
            bail!("experimental-image-proxy is not supported by this port")
        }
        "completion" => {
            let (shell, sub_matches) = matches
                .subcommand()
                .context("a shell is required (bash, zsh, fish or powershell)")?;
            completions::run(shell, sub_matches).map(|_| 0)
        }
        other => bail!("unrecognized command {other:?}"),
    }
}

/// Write bytes to stdout, failing on broken pipes like the Go tool does.
pub(crate) fn write_stdout(bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()?;
    Ok(())
}
