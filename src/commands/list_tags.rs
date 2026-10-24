//! `skopeo list-tags`.

use std::io::Read;

use anyhow::{bail, Context, Result};
use clap::ArgMatches;

use crate::cli::{self, GlobalOptions};
use crate::commands::write_stdout;
use crate::reference::{DockerReference, ImageReference};
use crate::util;

pub fn run(m: &ArgMatches, global: &GlobalOptions) -> Result<()> {
    let opts = cli::image_options_from(m, "", global, None)?;
    let positional: Vec<String> = m
        .get_many::<String>("SOURCE-IMAGE")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    if positional.len() != 1 {
        bail!("exactly one argument expected");
    }
    let name = &positional[0];

    let reference = ImageReference::parse(name)
        .with_context(|| format!("invalid {name:?}: does not specify a transport"))?;
    let tags = match &reference {
        ImageReference::Docker(docker_ref) => {
            if docker_ref.tag.is_some() || docker_ref.digest.is_some() {
                bail!("no tag or digest allowed in reference");
            }
            let mut src = crate::transports::open_source(&reference, &opts)?;
            src.tags()?
        }
        ImageReference::DockerArchive { path, .. } => docker_archive_tags(path)?,
        _ => bail!(
            "unsupported transport '{}' for tag listing. Only supported: archive, docker",
            reference.transport()
        ),
    };

    let mut report = serde_json::Map::new();
    if let Some(docker_ref) = reference.docker() {
        report.insert(
            "Repository".to_string(),
            serde_json::Value::String(docker_ref.name()),
        );
    }
    report.insert("Tags".to_string(), serde_json::to_value(&tags)?);
    let value = serde_json::Value::Object(report);
    write_stdout(util::go_json_string(&value)?.as_bytes())
}

/// Tags recorded in a `docker save` archive (unique, in archive order).
fn docker_archive_tags(path: &std::path::Path) -> Result<Vec<String>> {
    let reader = crate::archive::open_maybe_gzip(path)?;
    let mut tar = tar::Archive::new(reader);
    let mut tags: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in tar.entries().context("reading archive entries")? {
        let mut entry = entry.context("reading archive entry")?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let name = entry.path()?.to_path_buf();
        if name.file_name().and_then(|n| n.to_str()) != Some("manifest.json") {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        let manifests: serde_json::Value =
            serde_json::from_slice(&bytes).context("parsing docker-archive manifest.json")?;
        if let Some(list) = manifests.as_array() {
            for manifest in list {
                if let Some(repo_tags) = manifest.get("RepoTags").and_then(|t| t.as_array()) {
                    for tag in repo_tags {
                        if let Some(tag) = tag.as_str() {
                            if seen.insert(tag.to_string()) {
                                tags.push(tag.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    if tags.is_empty() {
        bail!("archive {}: no images found", path.display());
    }
    Ok(tags)
}

/// Guard shared with `delete`: only the docker transport supports this op.
pub fn require_docker(reference: &ImageReference) -> Result<&DockerReference> {
    match reference {
        ImageReference::Docker(r) => Ok(r),
        _ => bail!(
            "unsupported transport '{}' for this operation",
            reference.transport()
        ),
    }
}
