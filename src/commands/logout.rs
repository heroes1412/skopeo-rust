//! `skopeo logout`.

use anyhow::{bail, Result};
use clap::ArgMatches;

use crate::cli::GlobalOptions;
use crate::auth::{self, AuthFile};

pub fn run(m: &ArgMatches, _global: &GlobalOptions) -> Result<()> {
    let authfile = m
        .get_one::<String>("authfile")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let compat_auth_file = m
        .get_one::<String>("compat-auth-file")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let authfile = authfile.or(compat_auth_file);

    if m.get_flag("all") {
        if m.get_one::<String>("REGISTRY").is_some() {
            bail!("--all takes no arguments");
        }
        let mut file = AuthFile::load(authfile.as_deref())?;
        if let Some(object) = file.doc_mut().as_object_mut() {
            if let Some(auths) = object.get_mut("auths").and_then(|a| a.as_object_mut()) {
                auths.clear();
            }
            if let Some(helpers) = object.get_mut("credHelpers").and_then(|a| a.as_object_mut()) {
                helpers.clear();
            }
        }
        file.save()?;
        println!("Removed login credentials for all registries");
        return Ok(());
    }

    let positional: Vec<String> = m
        .get_many::<String>("REGISTRY")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    if positional.is_empty() {
        bail!("please provide a registry to log out from");
    }
    if positional.len() > 1 {
        bail!("logout accepts only one registry to log out from");
    }
    let key = auth::parse_credentials_key(&positional[0])?;

    let mut file = AuthFile::load(authfile.as_deref())?;
    if file.remove(&key) {
        file.save()?;
        println!("Removed login credentials for {key}");
        return Ok(());
    }

    // Not in the file: Go checks whether valid credentials exist anyway
    // (established by another tool, e.g. `docker login`).
    if let Some(stored) = file.lookup(&key) {
        if !stored.password.is_empty() {
            let opts = crate::options::ImageOptions::new();
            if crate::registry::try_login(&key, &stored.username, &stored.password, &opts).is_ok() {
                println!(
                    "Not logged into {key} with current tool. Existing credentials were established via docker login. Please use docker logout instead."
                );
                return Ok(());
            }
        }
    }
    bail!("not logged into {key}")
}
