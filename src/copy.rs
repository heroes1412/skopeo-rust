//! Transport-agnostic image copy engine.

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::digest::{self, Digest};
use crate::manifest::{
    convert_manifest, filter_index, select_platform, Descriptor, ManifestBytes, ManifestFormat,
    Platform,
};
use crate::options::ImageOptions;
use crate::reference::{DockerReference, ImageReference};
use crate::transports::{self, DestinationOptions, ImageDestination, ImageSource};
use crate::util;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiArch {
    System,
    All,
    IndexOnly,
    Platforms(Vec<Platform>),
}

impl MultiArch {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "system" => Ok(MultiArch::System),
            "all" => Ok(MultiArch::All),
            "index-only" => Ok(MultiArch::IndexOnly),
            other => {
                if !other.contains('/') {
                    bail!(
                        "unknown multi-arch option {other:?}. Choose one of the supported options: 'system', 'all', 'index-only', or a comma-separated platform list like 'linux/amd64,linux/arm64'"
                    );
                }
                let mut platforms = Vec::new();
                for entry in other.split(',') {
                    let entry = entry.trim();
                    let parts: Vec<&str> = entry.split('/').collect();
                    if parts.len() != 2 {
                        bail!(
                            "invalid platform format {entry:?} in --multi-arch, expected OS/Architecture (e.g., linux/amd64)"
                        );
                    }
                    platforms.push(Platform::new(parts[0], parts[1]));
                }
                Ok(MultiArch::Platforms(platforms))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionFormat {
    Gzip,
    Zstd,
}

impl CompressionFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "gzip" => Ok(CompressionFormat::Gzip),
            "zstd" => {
                #[cfg(not(feature = "zstd"))]
                bail!("compression format \"zstd\" requires building this port with the \"zstd\" feature enabled");
                #[cfg(feature = "zstd")]
                Ok(CompressionFormat::Zstd)
            }
            "zstd:chunked" => bail!("compression format \"zstd:chunked\" is not supported by this port"),
            other => bail!("unknown compression format {other:?} (expecting gzip or zstd)"),
        }
    }
}

/// What to do with layers on the way to the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerTransform {
    Keep,
    /// Compress to gzip/zstd unless the layer already uses the target format.
    Compress(CompressionFormat, Option<i32>),
    /// Decompress gzip/zstd layers to plain tar.
    Decompress,
    /// OCI destinations compress uncompressed layers by default.
    OciDefault,
}

#[derive(Clone, Debug)]
pub struct CopyOptions {
    pub src: ImageOptions,
    pub dest: ImageOptions,
    pub all: bool,
    pub multi_arch: Option<MultiArch>,
    pub format: Option<ManifestFormat>,
    pub preserve_digests: bool,
    pub remove_signatures: bool,
    pub remove_list_signatures: bool,
    pub download_foreign_layers: bool,
    pub digest_file: Option<std::path::PathBuf>,
    pub quiet: bool,
    pub additional_tags: Vec<DockerReference>,
    /// `--dest-compress` (dir/docker-archive destinations).
    pub dest_compress: bool,
    /// `--dest-decompress`.
    pub dest_decompress: bool,
    /// `--dest-compress-format` (implies gzip when `--dest-compress` is set
    /// without a format).
    pub dest_compress_format: Option<CompressionFormat>,
    /// `--dest-compress-level`.
    pub dest_compress_level: Option<i32>,
    /// `--dest-oci-accept-uncompressed-layers`.
    pub dest_oci_accept_uncompressed: bool,
}

impl CopyOptions {
    pub fn new(src: ImageOptions, dest: ImageOptions) -> Self {
        CopyOptions {
            src,
            dest,
            all: false,
            multi_arch: None,
            format: None,
            preserve_digests: false,
            remove_signatures: false,
            remove_list_signatures: false,
            download_foreign_layers: false,
            digest_file: None,
            quiet: false,
            additional_tags: Vec::new(),
            dest_compress: false,
            dest_decompress: false,
            dest_compress_format: None,
            dest_compress_level: None,
            dest_oci_accept_uncompressed: false,
        }
    }

    /// Resolve the per-destination layer transform, mirroring skopeo's rules:
    /// `--dest-compress`/`--dest-decompress` are honored only by `dir:`, OCI
    /// destinations compress uncompressed layers unless told not to, and
    /// other destinations warn and keep the source compression.
    pub fn layer_transform(&self, destination: &ImageReference) -> Result<LayerTransform> {
        if self.dest_compress && self.dest_decompress {
            bail!("Cannot compress and decompress at the same time");
        }
        let warn_ineffective = |flag: &str| {
            util::slog_line(
                "WARN",
                &format!("{flag} can only be used if the destination transport is 'dir'"),
                &[],
            );
        };
        match destination {
            ImageReference::Dir(_) => {
                if self.dest_decompress {
                    return Ok(LayerTransform::Decompress);
                }
                if self.dest_compress {
                    let format = self.dest_compress_format.unwrap_or(CompressionFormat::Gzip);
                    return Ok(LayerTransform::Compress(format, self.dest_compress_level));
                }
                Ok(LayerTransform::Keep)
            }
            ImageReference::Oci { .. } | ImageReference::OciArchive { .. } => {
                // Go still warns that the dir-only flags don't apply here;
                // OCI compression is driven by --dest-oci-accept-uncompressed-layers.
                if self.dest_compress {
                    warn_ineffective("--dest-compress");
                }
                if self.dest_decompress {
                    warn_ineffective("--dest-decompress");
                }
                if self.dest_oci_accept_uncompressed {
                    Ok(LayerTransform::Keep)
                } else {
                    Ok(LayerTransform::OciDefault)
                }
            }
            ImageReference::Docker(_) => {
                if self.dest_compress {
                    warn_ineffective("--dest-compress");
                }
                if self.dest_decompress {
                    warn_ineffective("--dest-decompress");
                }
                // The docker transport compresses uncompressed layers on push
                // (DesiredLayerCompression: Compress in containers/image).
                Ok(LayerTransform::OciDefault)
            }
            _ => {
                if self.dest_compress {
                    warn_ineffective("--dest-compress");
                }
                if self.dest_decompress {
                    warn_ineffective("--dest-decompress");
                }
                Ok(LayerTransform::Keep)
            }
        }
    }
}

/// Copy `source` to `destination`, returning the digest of the manifest that
/// was written.
pub fn copy_image(
    source: &ImageReference,
    destination: &ImageReference,
    opts: &CopyOptions,
) -> Result<String> {
    if opts.all && opts.multi_arch.is_some() {
        bail!("cannot use --all and --multi-arch flags together");
    }
    if opts.remove_signatures && opts.remove_list_signatures {
        bail!("only one of --remove-signatures and --remove-list-signatures can be specified");
    }

    let mut src = transports::open_source(source, &opts.src)?;
    let mut dest_opts = DestinationOptions {
        source_reference: source.docker().cloned(),
        additional_tags: opts.additional_tags.clone(),
        compress: None,
    };
    if dest_opts.source_reference.is_none() {
        dest_opts.source_reference = src.docker_reference().cloned();
    }
    let mut dest = transports::open_destination(destination, &opts.dest, &dest_opts)?;

    let progress = Progress::new(opts.quiet);

    let transform = opts.layer_transform(destination)?;
    let top = src.manifest().context("reading source manifest")?;
    if top.is_schema1() {
        bail!("schema1 manifests are not supported by this port");
    }

    let target_format = target_format(destination, opts.format);
    let (written_digest, written_bytes) = if top.is_index() {
        let selection = effective_selection(opts);
        if !dest.supports_multi_arch() && selection != MultiArch::System {
            bail!(
                "copying multiple images: destination transport {:?} does not support copying multiple images as a group",
                destination.transport()
            );
        }
        progress.status("Getting image list signatures");
        let ctx = CopyContext {
            format: target_format,
            transform,
            opts,
            progress: &progress,
        };
        copy_index(&mut *src, &mut *dest, &top, &selection, &ctx)?
    } else {
        progress.status("Getting image source signatures");
        let ctx = CopyContext {
            format: target_format,
            transform,
            opts,
            progress: &progress,
        };
        let instance = copy_single(&mut *src, &mut *dest, &top, None, &ctx)?;
        (instance.digest, instance.bytes)
    };

    dest.commit().context("committing image destination")?;

    if let Some(path) = &opts.digest_file {
        util::atomic_write(path, written_digest.as_bytes())
            .with_context(|| format!("writing digest to {}", path.display()))?;
    }
    let _ = written_bytes;
    Ok(written_digest)
}

/// Shared parameters threaded through the copy engine.
struct CopyContext<'a> {
    format: Option<ManifestFormat>,
    transform: LayerTransform,
    opts: &'a CopyOptions,
    progress: &'a Progress,
}

struct WrittenInstance {
    digest: String,
    size: u64,
    media_type: String,
    bytes: Vec<u8>,
    original_digest: String,
}

fn effective_selection(opts: &CopyOptions) -> MultiArch {
    if let Some(selection) = &opts.multi_arch {
        selection.clone()
    } else if opts.all {
        MultiArch::All
    } else {
        MultiArch::System
    }
}

fn target_format(
    destination: &ImageReference,
    requested: Option<ManifestFormat>,
) -> Option<ManifestFormat> {
    if let Some(format) = requested {
        return Some(format);
    }
    match destination {
        // The docker transport accepts OCI and v2s2 manifests (and lists), so
        // the source type is preserved and digests stay stable. The oci
        // transport only accepts OCI manifests, so conversion happens there.
        ImageReference::Docker(_) | ImageReference::Dir(_) | ImageReference::DockerArchive { .. } => {
            None
        }
        ImageReference::Oci { .. } | ImageReference::OciArchive { .. } => Some(ManifestFormat::Oci),
    }
}

fn copy_index(
    src: &mut dyn ImageSource,
    dest: &mut dyn ImageDestination,
    top: &ManifestBytes,
    selection: &MultiArch,
    ctx: &CopyContext<'_>,
) -> Result<(String, Vec<u8>)> {
    let (format, opts, progress) = (ctx.format, ctx.opts, ctx.progress);
    let index = top.parse_index().context("parsing manifest list")?;

    let descriptors: Vec<Descriptor> = match selection {
        MultiArch::System => {
            let wanted = Platform {
                architecture: opts.src.arch.clone(),
                os: opts.src.os.clone(),
                variant: opts.src.variant.clone(),
                ..Default::default()
            };
            vec![select_platform(&index, &wanted)?]
        }
        MultiArch::IndexOnly => Vec::new(),
        MultiArch::All => index.manifests.clone(),
        MultiArch::Platforms(platforms) => {
            let filtered = filter_index(&index, platforms)?;
            filtered.manifests
        }
    };

    if *selection != MultiArch::IndexOnly {
        progress.status(&format!(
            "Copying {} images generated from {} images in list",
            descriptors.len(),
            index.manifests.len()
        ));
    }
    let mut written_children: Vec<WrittenInstance> = Vec::new();
    for (position, descriptor) in descriptors.iter().enumerate() {
        let child = src
            .manifest_for_digest(&descriptor.digest)
            .with_context(|| format!("reading instance manifest {}", descriptor.digest))?;
        progress.status(&format!(
            "Copying image {} ({}/{})",
            descriptor.digest,
            position + 1,
            descriptors.len()
        ));
        let written = copy_single(src, dest, &child, Some(&descriptor.digest), ctx)?;
        written_children.push(written);
    }

    // Rebuild the index so that it points at the manifests we actually wrote
    // (conversion changes their digests).
    let mut index_value: Value = serde_json::from_slice(&top.bytes)?;
    if !written_children.is_empty() {
        let replacements: std::collections::HashMap<&str, &WrittenInstance> = written_children
            .iter()
            .map(|child| (child.original_digest.as_str(), child))
            .collect();
        if let Some(manifests) = index_value.get_mut("manifests").and_then(Value::as_array_mut) {
            for descriptor in manifests.iter_mut() {
                let Some(object) = descriptor.as_object_mut() else {
                    continue;
                };
                let Some(old_digest) = object.get("digest").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(replacement) = replacements.get(old_digest) {
                    object.insert(
                        "digest".to_string(),
                        Value::String(replacement.digest.clone()),
                    );
                    object.insert("size".to_string(), Value::from(replacement.size));
                    if !replacement.media_type.is_empty() {
                        object.insert(
                            "mediaType".to_string(),
                            Value::String(replacement.media_type.clone()),
                        );
                    }
                }
            }
        }
        if matches!(selection, MultiArch::Platforms(_)) {
            let keep: std::collections::HashSet<&str> = written_children
                .iter()
                .map(|c| c.digest.as_str())
                .collect();
            if let Some(manifests) = index_value.get_mut("manifests").and_then(Value::as_array_mut) {
                manifests.retain(|descriptor| {
                    descriptor
                        .get("digest")
                        .and_then(Value::as_str)
                        .map(|digest| keep.contains(digest))
                        .unwrap_or(false)
                });
            }
        }
    } else if matches!(selection, MultiArch::System | MultiArch::IndexOnly) {
        // Index-only copies keep the list untouched.
    }

    let index_bytes = serde_json::to_vec(&index_value)?;
    let (final_bytes, final_media_type) = match format {
        Some(format) => convert_manifest(&index_bytes, format)?,
        None => (index_bytes, top.media_type.clone()),
    };

    let written = dest
        .put_manifest(&final_bytes, &final_media_type, None)
        .context("writing manifest list to image destination")?;
    Ok((written, final_bytes))
}

fn copy_single(
    src: &mut dyn ImageSource,
    dest: &mut dyn ImageDestination,
    manifest: &ManifestBytes,
    instance: Option<&str>,
    ctx: &CopyContext<'_>,
) -> Result<WrittenInstance> {
    let (format, transform, opts, progress) = (ctx.format, ctx.transform, ctx.opts, ctx.progress);
    if manifest.is_schema1() {
        bail!("schema1 manifests are not supported by this port");
    }
    let parsed = manifest.parse_manifest().context("parsing image manifest")?;

    // Layers, rewriting the manifest when a transformation changes a blob.
    let mut manifest_value: Option<Value> = None;
    for (index, layer) in parsed.layers.iter().enumerate() {
        if layer.is_foreign()
            && !opts.download_foreign_layers
            && dest.accepts_foreign_layer_urls()
            && layer.urls.is_some()
        {
            if opts.src.debug {
                eprintln!("Skipping foreign layer {:?} copy", layer.digest);
            }
            continue;
        }
        let from = blob_compression(&layer.media_type);
        let target: Option<CompressionFormat> = match transform {
            LayerTransform::Keep => None,
            LayerTransform::OciDefault => match from {
                Some(_) => None,
                None => Some(CompressionFormat::Gzip),
            },
            LayerTransform::Decompress => match from {
                Some(_) => None, // decompression handled below
                None => None,
            },
            LayerTransform::Compress(target, _) => match from {
                Some(existing) if compression_matches(existing, target) => None,
                _ => Some(target),
            },
        };
        if transform == LayerTransform::Decompress && from.is_some() {
            let (new_digest, new_size, new_media_type) =
                transcode_layer(src, dest, layer, None, opts, progress)?;
            rewrite_layer(&mut manifest_value, &manifest.bytes, index, &new_digest, new_size, &new_media_type);
            continue;
        }
        match target {
            None => copy_blob(src, dest, layer, opts, progress, false)?,
            Some(compression) => {
                let (new_digest, new_size, new_media_type) =
                    transcode_layer(src, dest, layer, Some(compression), opts, progress)?;
                rewrite_layer(&mut manifest_value, &manifest.bytes, index, &new_digest, new_size, &new_media_type);
            }
        }
    }

    // The config blob is copied after the layers, like the Go engine does.
    copy_blob(src, dest, &parsed.config, opts, progress, true)?;

    // Serialize the manifest, converting formats if requested.
    let (bytes, media_type) = match (&manifest_value, format) {
        (Some(value), Some(fmt)) => {
            let raw = serde_json::to_vec(value)?;
            convert_manifest(&raw, fmt).context("converting manifest format")?
        }
        (Some(value), None) => {
            let raw = serde_json::to_vec(value)?;
            (raw, manifest.media_type.clone())
        }
        (None, Some(fmt)) => {
            convert_manifest(&manifest.bytes, fmt).context("converting manifest format")?
        }
        (None, None) => (manifest.bytes.clone(), manifest.media_type.clone()),
    };
    // Children of a list are PUT under the digest of the bytes actually
    // written (Go replaces instanceDigest with the post-conversion digest).
    let written_instance_digest = instance.map(|_| digest::digest_bytes(&bytes));
    progress.status("Writing manifest to image destination");
    let digest = dest.put_manifest(&bytes, &media_type, written_instance_digest.as_deref())?;
    Ok(WrittenInstance {
        digest,
        size: bytes.len() as u64,
        media_type,
        bytes,
        original_digest: manifest.digest(),
    })
}

fn rewrite_layer(
    manifest_value: &mut Option<Value>,
    original: &[u8],
    index: usize,
    digest: &str,
    size: u64,
    media_type: &str,
) {
    let value = manifest_value
        .get_or_insert_with(|| serde_json::from_slice(original).expect("manifest already parsed"));
    if let Some(descriptor) = value
        .get_mut("layers")
        .and_then(Value::as_array_mut)
        .and_then(|layers| layers.get_mut(index))
        .and_then(Value::as_object_mut)
    {
        descriptor.insert("digest".to_string(), Value::String(digest.to_string()));
        descriptor.insert("size".to_string(), Value::from(size));
        descriptor.insert("mediaType".to_string(), Value::String(media_type.to_string()));
    }
}

fn copy_blob(
    src: &mut dyn ImageSource,
    dest: &mut dyn ImageDestination,
    descriptor: &Descriptor,
    _opts: &CopyOptions,
    progress: &Progress,
    is_config: bool,
) -> Result<()> {
    let digest = Digest::parse(&descriptor.digest)
        .with_context(|| format!("invalid blob digest {:?}", descriptor.digest))?;
    let kind = if is_config { "config" } else { "blob" };
    // Go prints the copy line before the reuse check, so reused blobs still
    // appear, and without a size suffix on the non-TTY path.
    progress.status(&format!("Copying {kind} {digest}"));
    if dest.has_blob(&digest)? {
        return Ok(());
    }

    let mut blob = match src.open_blob(&digest) {
        Ok(blob) => blob,
        Err(err) => {
            // Foreign layers are not stored on the source registry; fetch
            // them from their advertised URLs instead.
            if let Some(urls) = &descriptor.urls {
                if descriptor.is_foreign() {
                    for url in urls {
                        if let Ok(mut reader) = open_foreign_url(url) {
                            let size = if descriptor.size > 0 {
                                Some(descriptor.size as u64)
                            } else {
                                None
                            };
                            return dest
                                .put_blob(&digest, size, &mut reader)
                                .with_context(|| format!("copying {kind} {digest}"));
                        }
                    }
                }
            }
            return Err(err).with_context(|| format!("reading {kind} {digest}"));
        }
    };
    let size = blob.size.or(if descriptor.size > 0 {
        Some(descriptor.size as u64)
    } else {
        None
    });
    dest.put_blob(&digest, size, &mut blob.reader)
        .with_context(|| format!("copying {kind} {digest}"))?;
    Ok(())
}

/// Fetch a foreign-layer blob from one of its advertised URLs. The reader is
/// leaked so it satisfies the `&mut dyn Read` destination API; copy
/// operations are process-lifetime bounded.
fn open_foreign_url(url: &str) -> Result<Box<dyn std::io::Read>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().build().into();
    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("fetching foreign layer from {url}"))?;
    if !response.status().is_success() {
        bail!(
            "fetching foreign layer from {url}: HTTP {}",
            response.status()
        );
    }
    Ok(Box::new(response.into_body().into_reader()))
}

/// Which compression a layer media type uses.
fn blob_compression(media_type: &str) -> Option<CompressionFormat> {
    if media_type.contains("gzip") {
        Some(CompressionFormat::Gzip)
    } else if media_type.contains("zstd") {
        Some(CompressionFormat::Zstd)
    } else {
        None
    }
}

fn compression_matches(existing: CompressionFormat, target: CompressionFormat) -> bool {
    existing == target
}

/// Uncompressed media type for a compressed one, or vice versa.
fn transformed_media_type(media_type: &str, target: Option<CompressionFormat>) -> String {
    let base = media_type
        .replace("+gzip", "")
        .replace(".gzip", "")
        .replace("+zstd", "")
        .replace(".zstd", "");
    match (target, is_docker_media_type(media_type)) {
        (Some(CompressionFormat::Gzip), true) => format!("{base}.gzip"),
        (Some(CompressionFormat::Gzip), false) => format!("{base}+gzip"),
        (Some(CompressionFormat::Zstd), _) => format!("{base}+zstd"),
        (None, _) => base,
    }
}

fn is_docker_media_type(media_type: &str) -> bool {
    media_type.contains("docker")
}

/// Decompress/recompress a layer, spooling through a temporary file so that
/// arbitrarily large layers never hit memory. Returns the new digest, size
/// and media type; `target == None` means decompress to plain tar.
fn transcode_layer(
    src: &mut dyn ImageSource,
    dest: &mut dyn ImageDestination,
    layer: &Descriptor,
    target: Option<CompressionFormat>,
    opts: &CopyOptions,
    _progress: &Progress,
) -> Result<(String, u64, String)> {
    let old_digest = Digest::parse(&layer.digest)
        .with_context(|| format!("invalid blob digest {:?}", layer.digest))?;
    let from = blob_compression(&layer.media_type);
    let new_media_type = transformed_media_type(&layer.media_type, target);

    let blob = src
        .open_blob(&old_digest)
        .with_context(|| format!("reading blob {old_digest}"))?;

    let temp_dir = util::temp_dir(opts.dest.tmpdir.as_deref(), "skopeo-transcode")?;
    let temp_path = temp_dir.path().join("layer");
    let temp_file = std::fs::File::create(&temp_path)
        .with_context(|| format!("creating {}", temp_path.display()))?;

    let mut decoder: Box<dyn std::io::Read> = match from {
        Some(CompressionFormat::Gzip) => Box::new(flate2::read::MultiGzDecoder::new(blob.reader)),
        Some(CompressionFormat::Zstd) => {
            #[cfg(feature = "zstd")]
            {
                Box::new(zstd::stream::read::Decoder::new(blob.reader)?)
            }
            #[cfg(not(feature = "zstd"))]
            bail!(
                "layer {old_digest} uses zstd compression, but this binary was built without zstd support"
            )
        }
        None => Box::new(blob.reader),
    };

    // Stream: blob -> (decoder) -> (encoder) -> hashing writer -> temp file.
    let mut hasher = crate::digest::HashingWriter::new(temp_file);
    match target {
        Some(CompressionFormat::Gzip) => {
            let level = opts.dest_compress_level.unwrap_or(6).clamp(0, 9) as u32;
            let mut encoder = flate2::write::GzEncoder::new(&mut hasher, flate2::Compression::new(level));
            std::io::copy(&mut decoder, &mut encoder)
                .with_context(|| format!("recompressing layer {old_digest}"))?;
            encoder
                .finish()
                .and_then(|w| w.flush())
                .with_context(|| format!("finishing recompression of layer {old_digest}"))?;
        }
        Some(CompressionFormat::Zstd) => {
            #[cfg(feature = "zstd")]
            {
                let level = opts.dest_compress_level.unwrap_or(3);
                let mut encoder =
                    zstd::stream::write::Encoder::new(&mut hasher, level)
                        .with_context("creating zstd encoder")?;
                std::io::copy(&mut decoder, &mut encoder)
                    .with_context(|| format!("recompressing layer {old_digest}"))?;
                encoder
                    .finish()
                    .and_then(|_| hasher.flush())
                    .with_context(|| format!("finishing recompression of layer {old_digest}"))?;
            }
            #[cfg(not(feature = "zstd"))]
            bail!("zstd output requires building this port with the \"zstd\" feature enabled")
        }
        None => {
            std::io::copy(&mut decoder, &mut hasher)
                .with_context(|| format!("decompressing layer {old_digest}"))?;
            hasher.flush()?;
        }
    }
    let encoded = hasher.finish_digest();
    let new_size = hasher.bytes_written();
    let new_digest = format!("sha256:{encoded}");

    // Upload from the spooled file.
    let mut spooled = std::fs::File::open(&temp_path)
        .with_context(|| format!("reading spooled layer {}", temp_path.display()))?;
    dest.put_blob(
        &Digest::parse(&new_digest)?,
        Some(new_size),
        &mut spooled,
    )
    .with_context(|| format!("copying blob {new_digest}"))?;
    Ok((new_digest, new_size, new_media_type))
}

struct Progress {
    quiet: bool,
}

impl Progress {
    fn new(quiet: bool) -> Self {
        Progress { quiet }
    }

    /// skopeo writes copy progress to stdout.
    fn status(&self, message: &str) {
        if !self.quiet {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{message}");
            let _ = stdout.flush();
        }
    }

}

/// Compute the digest of a manifest file, matching `skopeo manifest-digest`
/// (schema1 signatures are not part of the canonical digest).
pub fn manifest_digest_from_bytes(bytes: &[u8]) -> Result<String> {
    let mut value: Value = serde_json::from_slice(bytes).context("parsing manifest")?;
    let schema1 = value
        .get("schemaVersion")
        .and_then(Value::as_i64)
        .map(|v| v == 1)
        .unwrap_or(false)
        || value
            .get("mediaType")
            .and_then(Value::as_str)
            .map(|m| m.contains("docker.distribution.manifest.v1"))
            .unwrap_or(false);
    if schema1 {
        if let Some(object) = value.as_object_mut() {
            object.remove("signatures");
        }
        let canonical = serde_json::to_vec(&value)?;
        return Ok(digest::digest_bytes(&canonical));
    }
    Ok(digest::digest_bytes(bytes))
}

/// Convenience wrapper for the CLI: parse both images, then copy.
pub fn copy_image_names(
    source: &str,
    destination: &str,
    opts: &CopyOptions,
) -> Result<String> {
    let source = ImageReference::parse(source)
        .with_context(|| format!("invalid source name {source}"))?;
    let destination = ImageReference::parse(destination)
        .with_context(|| format!("invalid destination name {destination}"))?;
    copy_image(&source, &destination, opts)
}

/// Read a manifest from a file (for `manifest-digest`).
pub fn manifest_digest_from_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading manifest from {}", path.display()))?;
    manifest_digest_from_bytes(&bytes).with_context(|| format!("computing digest of {}", path.display()))
}
