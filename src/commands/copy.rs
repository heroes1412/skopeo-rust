//! `skopeo copy`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::ArgMatches;

use crate::cli::{self, GlobalOptions};
use crate::copy::{CopyOptions, MultiArch};
use crate::manifest::ManifestFormat;
use crate::options::ImageOptions;
use crate::reference::DockerReference;

pub fn run(m: &ArgMatches, global: &GlobalOptions) -> Result<()> {
    let mut positional: Vec<String> = Vec::new();
    if let Some(values) = m.get_many::<String>("SOURCE-IMAGE") {
        positional.extend(values.cloned());
    }
    if let Some(values) = m.get_many::<String>("DESTINATION-IMAGE") {
        positional.extend(values.cloned());
    }
    if positional.len() != 2 {
        bail!("exactly two arguments expected");
    }

    let src_opts = with_retry(
        cli::image_options_from(m, "src-", global, Some("tls-verify"))?,
        cli::retry_options(m)?,
    );
    let dest_opts = with_retry(
        cli::image_options_from(m, "dest-", global, Some("tls-verify"))?,
        cli::retry_options(m)?,
    );

    let mut opts = CopyOptions::new(src_opts, dest_opts);
    opts.quiet = m.get_flag("quiet");
    opts.all = m.get_flag("all");
    if let Some(multi_arch) = m.get_one::<String>("multi-arch") {
        opts.multi_arch = Some(MultiArch::parse(multi_arch)?);
    }
    if let Some(format) = m.get_one::<String>("format").filter(|v| !v.is_empty()) {
        opts.format = Some(ManifestFormat::parse(format)?);
    }
    opts.preserve_digests = m.get_flag("preserve-digests");
    opts.remove_signatures = m.get_flag("remove-signatures");
    opts.remove_list_signatures = m.get_flag("remove-list-signatures");
    opts.download_foreign_layers = m.get_flag("download-foreign-layers");
    if let Some(path) = m.get_one::<String>("digestfile").filter(|v| !v.is_empty()) {
        opts.digest_file = Some(PathBuf::from(path));
    }
    for tag in m.get_many::<String>("additional-tag").unwrap_or_default() {
        let reference = DockerReference::parse(tag).with_context(|| {
            format!("parsing additional-tag '{tag}'")
        })?;
        if reference.digest.is_some() {
            bail!("additional-tag '{tag}' must be a tagged reference");
        }
        opts.additional_tags.push(reference);
    }
    let encryption_keys: Vec<&String> = m.get_many::<String>("encryption-key").unwrap_or_default().collect();
    let decryption_keys: Vec<&String> = m.get_many::<String>("decryption-key").unwrap_or_default().collect();
    if !encryption_keys.is_empty() && !decryption_keys.is_empty() {
        bail!("--encryption-key and --decryption-key cannot be specified together");
    }
    if !encryption_keys.is_empty() || !decryption_keys.is_empty() {
        bail!("image encryption/decryption is not supported by this port");
    }
    if m.get_many::<String>("encrypt-layer").map(|v| v.count()).unwrap_or(0) > 0
        && encryption_keys.is_empty()
    {
        bail!("--encrypt-layer can only be used with --encryption-key");
    }
    for flag in [
        "sign-by",
        "sign-by-sq-fingerprint",
        "sign-by-sigstore",
        "sign-by-sigstore-private-key",
    ] {
        if let Some(value) = m.get_one::<String>(flag).filter(|v| !v.is_empty()) {
            bail!("signing with {flag}={value:?} is not supported by this port");
        }
    }
    if let Some(identity) = m.get_one::<String>("sign-identity").filter(|v| !v.is_empty()) {
        DockerReference::parse(identity)
            .with_context(|| format!("parsing --sign-identity: {identity:?}"))?;
        bail!("signing is not supported by this port");
    }
    // Blob existence is checked before every upload, which is what
    // --dest-precompute-digests optimizes for; --image-parallel-copies and
    // --strip-removed-platforms only tune Go-side behavior that has no
    // equivalent in this sequential port.
    let _ = m.get_one::<String>("image-parallel-copies");
    let _ = m.get_flag("strip-removed-platforms");

    // Destination compression options (dir: destination).
    opts.dest_compress = m.get_flag("dest-compress");
    opts.dest_decompress = m.get_flag("dest-decompress");
    opts.dest_oci_accept_uncompressed = m.get_flag("dest-oci-accept-uncompressed-layers");
    if let Some(format) = m
        .get_one::<String>("dest-compress-format")
        .filter(|v| !v.is_empty())
    {
        opts.dest_compress_format = Some(crate::copy::CompressionFormat::parse(format)?);
    }
    opts.dest_compress_level = m.get_one::<i64>("dest-compress-level").map(|v| *v as i32);

    let source = &positional[0];
    let destination = &positional[1];
    crate::copy::copy_image_names(source, destination, &opts)?;
    Ok(())
}

fn with_retry(mut opts: ImageOptions, (retries, delay): (u32, Option<std::time::Duration>)) -> ImageOptions {
    opts.retries = retries;
    opts.retry_delay = delay;
    opts
}
