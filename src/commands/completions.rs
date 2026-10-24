//! `skopeo completion <shell>` (hidden): generate shell completions.

use anyhow::{bail, Result};
use clap::ArgMatches;
use clap_complete::{generate, Shell};

use crate::cli;

pub fn run(shell_name: &str, _m: &ArgMatches) -> Result<()> {
    let shell = match shell_name {
        "bash" => Shell::Bash,
        "zsh" => Shell::Zsh,
        "fish" => Shell::Fish,
        "powershell" => Shell::PowerShell,
        other => bail!("unsupported shell {other:?}"),
    };
    let mut cmd = cli::build_cli();
    let mut buffer = Vec::new();
    generate(shell, &mut cmd, "skopeo", &mut buffer);
    // Ignore broken pipes (`skopeo completion bash | head`).
    use std::io::Write;
    let _ = std::io::stdout().write_all(&buffer);
    let _ = std::io::stdout().flush();
    Ok(())
}
