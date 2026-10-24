//! `skopeo delete`.
//!
//! Like the Go implementation, the manifest is resolved first and the
//! deletion is issued against its digest (many registries only implement
//! delete-by-digest), using token scope `*`.

use anyhow::{Context, Result};
use clap::ArgMatches;

use crate::cli::{self, GlobalOptions};
use crate::registry::{HttpStatusError, RegistryClient};
use crate::reference::ImageReference;

pub fn run(m: &ArgMatches, global: &GlobalOptions) -> Result<()> {
    let opts = cli::image_options_from(m, "", global, None)?;
    let positional: Vec<String> = m
        .get_many::<String>("IMAGE-NAME")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    if positional.len() != 1 {
        bail!("exactly one argument expected");
    }
    let name = &positional[0];

    let reference = ImageReference::parse(name)?;
    let docker_ref = super::list_tags::require_docker(&reference)?;
    let mut client = RegistryClient::new(docker_ref, &opts, "*")?;

    // Resolve the manifest, then delete by its computed digest.
    let response = match client.get_manifest(&docker_ref.reference()) {
        Ok(response) => response,
        Err(err) => {
            let not_found = err
                .downcast_ref::<HttpStatusError>()
                .map(|e| e.status == 404)
                .unwrap_or(false);
            if not_found {
                // Keep the 404 cause in the chain so main maps it to exit
                // code 2, like skopeo's isNotFoundImageError.
                return Err(anyhow::Error::new(HttpStatusError {
                    status: 404,
                    message: "manifest unknown".to_string(),
                })
                .context(format!(
                    "Unable to delete {}. Image may not exist or is not stored with a v2 Schema in a v2 registry",
                    docker_ref.familiar_name()
                )));
            }
            return Err(err).context("reading manifest");
        }
    };
    let digest = response
        .digest
        .clone()
        .unwrap_or_else(|| crate::digest::digest_bytes(&response.manifest.bytes));
    client.delete_manifest(&digest).context("deleting manifest")
}

use anyhow::bail;
