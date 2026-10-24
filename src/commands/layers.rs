//! `skopeo layers` (hidden, deprecated): download image blobs into a
//! `layers-*` directory using the `dir:` destination layout.

use anyhow::{Context, Result};
use clap::ArgMatches;

use crate::cli::{self, GlobalOptions};
use crate::digest::Digest;
use crate::reference::ImageReference;
use crate::transports::{open_source, ImageDestination};

pub fn run(m: &ArgMatches, global: &GlobalOptions) -> Result<()> {
    eprintln!("DEPRECATED: skopeo layers is deprecated in favor of skopeo copy");
    let opts = cli::image_options_from(m, "", global, None)?;
    let name = m.get_one::<String>("IMAGE-NAME").expect("required");
    let wanted: Vec<String> = m
        .get_many::<String>("LAYERS")
        .map(|values| values.cloned().collect::<Vec<String>>())
        .unwrap_or_default()
        .into_iter()
        .map(|d| {
            if d.contains(':') {
                d
            } else {
                format!("sha256:{d}")
            }
        })
        .collect();
    for digest in &wanted {
        Digest::parse(digest).with_context(|| format!("invalid layer digest {digest:?}"))?;
    }

    let reference = ImageReference::parse(name)?;
    let mut src = open_source(&reference, &opts)?;

    let top = src.manifest().context("reading source manifest")?;
    let parsed = top.parse_manifest()?;

    let _dir = std::env::current_dir()?.join("layers-0");
    let mut suffix = 0;
    let dir = loop {
        let candidate = std::env::current_dir()?
            .join(format!("layers-{suffix}"));
        if !candidate.exists() {
            break candidate;
        }
        suffix += 1;
    };
    std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut dest = crate::transports::dir::DirDestination::new(&dir)?;

    // With explicit layer arguments only those blobs are fetched (no config);
    // otherwise the config blob comes after all layers, like Go.
    let mut descriptors: Vec<&crate::manifest::Descriptor> = Vec::new();
    if wanted.is_empty() {
        descriptors.extend(parsed.layers.iter());
        descriptors.push(&parsed.config);
    } else {
        for layer in &parsed.layers {
            if wanted.contains(&layer.digest) {
                descriptors.push(layer);
            }
        }
    }
    for descriptor in descriptors {
        let digest = Digest::parse(&descriptor.digest)?;
        if dest.has_blob(&digest)? {
            continue;
        }
        let mut blob = src.open_blob(&digest)?;
        dest.put_blob(&digest, blob.size, &mut blob.reader)?;
    }
    dest.put_manifest(&top.bytes, &top.media_type, None)?;
    dest.commit()?;
    Ok(())
}
