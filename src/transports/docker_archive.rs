//! The `docker-archive:` transport (`docker save` / `docker load` format).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::archive;
use crate::digest::{self, Digest};
use crate::manifest::{
    ManifestBytes, Manifest, MEDIA_TYPE_DOCKER_CONFIG, MEDIA_TYPE_DOCKER_LAYER,
    MEDIA_TYPE_DOCKER_LAYER_GZIP, MEDIA_TYPE_DOCKER_MANIFEST,
};
use crate::options::ImageOptions;
use crate::reference::DockerReference;
use crate::transports::{BlobReader, DestinationOptions, ImageDestination, ImageSource};
use crate::util;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SaveItem {
    #[serde(rename = "Config", default)]
    config: String,
    #[serde(rename = "RepoTags", default)]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Layers", default)]
    layers: Option<Vec<String>>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

pub struct DockerArchiveSource {
    _temp: tempfile::TempDir,
    manifest: ManifestBytes,
    blobs: BTreeMap<String, PathBuf>,
    tags: Vec<String>,
    docker_reference: Option<DockerReference>,
}

impl DockerArchiveSource {
    pub fn new(
        path: &Path,
        reference: Option<&DockerReference>,
        opts: &ImageOptions,
    ) -> Result<Self> {
        let temp = util::temp_dir(opts.tmpdir.as_deref(), "skopeo-docker-archive-")?;
        archive::extract_to_dir(path, temp.path())
            .with_context(|| format!("extracting docker archive {}", path.display()))?;

        let manifest_path = temp.path().join("manifest.json");
        if !manifest_path.exists() {
            bail!(
                "{} is not a docker archive: manifest.json is missing",
                path.display()
            );
        }
        let items: Vec<SaveItem> = serde_json::from_slice(&fs::read(&manifest_path)?)
            .context("parsing docker archive manifest.json")?;
        if items.is_empty() {
            bail!("docker archive {} contains no images", path.display());
        }

        let mut tags = Vec::new();
        for (index, item) in items.iter().enumerate() {
            match &item.repo_tags {
                Some(list) if !list.is_empty() => tags.extend(list.iter().cloned()),
                _ => tags.push(format!("@{index}")),
            }
        }

        let chosen_index = match reference {
            Some(wanted) => items
                .iter()
                .position(|item| {
                    item.repo_tags
                        .as_ref()
                        .map(|tags| {
                            tags.iter().any(|tag| {
                                DockerReference::parse(tag)
                                    .map(|r| r.familiar_name() == wanted.familiar_name())
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                })
                .with_context(|| {
                    format!(
                        "image {} not found in docker archive {} (tags: {})",
                        wanted.familiar_name(),
                        path.display(),
                        tags.join(", ")
                    )
                })?,
            None => 0,
        };
        let item = &items[chosen_index];

        let chosen_docker_ref = item
            .repo_tags
            .as_ref()
            .and_then(|tags| tags.first())
            .and_then(|tag| DockerReference::parse(tag).ok());

        let mut blobs = BTreeMap::new();

        // Config blob.
        if item.config.is_empty() {
            bail!("docker archive entry has no Config");
        }
        let config_path = temp.path().join(&item.config);
        let config_bytes = fs::read(&config_path)
            .with_context(|| format!("reading config {}", config_path.display()))?;
        let config_digest = digest::digest_bytes(&config_bytes);
        blobs.insert(config_digest.clone(), config_path.clone());

        // Layers.
        let layer_paths = item.layers.clone().unwrap_or_default();
        let mut layer_descriptors = Vec::new();
        for layer in &layer_paths {
            let layer_path = temp.path().join(layer);
            let (digest_string, size, gzip) = hash_file(&layer_path)?;
            blobs.insert(digest_string.clone(), layer_path.clone());
            let media_type = if gzip {
                MEDIA_TYPE_DOCKER_LAYER_GZIP
            } else {
                MEDIA_TYPE_DOCKER_LAYER
            };
            layer_descriptors.push(json!({
                "mediaType": media_type,
                "size": size,
                "digest": digest_string,
            }));
        }

        let manifest_value = json!({
            "schemaVersion": 2,
            "mediaType": MEDIA_TYPE_DOCKER_MANIFEST,
            "config": {
                "mediaType": MEDIA_TYPE_DOCKER_CONFIG,
                "size": config_bytes.len(),
                "digest": config_digest,
            },
            "layers": layer_descriptors,
        });
        let manifest_bytes = serde_json::to_vec(&manifest_value)?;

        Ok(DockerArchiveSource {
            _temp: temp,
            manifest: ManifestBytes {
                bytes: manifest_bytes,
                media_type: MEDIA_TYPE_DOCKER_MANIFEST.to_string(),
            },
            blobs,
            tags,
            docker_reference: chosen_docker_ref,
        })
    }

    fn blob_path(&self, digest: &Digest) -> Result<&PathBuf> {
        self.blobs
            .get(&digest.to_string())
            .with_context(|| format!("blob {digest} is not present in the docker archive"))
    }
}

impl ImageSource for DockerArchiveSource {
    fn manifest(&mut self) -> Result<ManifestBytes> {
        Ok(self.manifest.clone())
    }

    fn manifest_for_digest(&mut self, _digest: &str) -> Result<ManifestBytes> {
        bail!("docker-archive does not support manifest lists")
    }

    fn open_blob(&mut self, digest: &Digest) -> Result<BlobReader> {
        let path = self.blob_path(digest)?;
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let size = file.metadata().ok().map(|m| m.len());
        Ok(BlobReader {
            reader: Box::new(file),
            size,
        })
    }

    fn tags(&mut self) -> Result<Vec<String>> {
        Ok(self.tags.clone())
    }

    fn docker_reference(&self) -> Option<&DockerReference> {
        self.docker_reference.as_ref()
    }
}

pub struct DockerArchiveDestination {
    path: PathBuf,
    temp: tempfile::TempDir,
    blobs: PathBuf,
    repo_tags: Vec<String>,
    manifest: Option<ManifestBytes>,
}

impl DockerArchiveDestination {
    pub fn new(
        path: &Path,
        reference: Option<&DockerReference>,
        dest_opts: &DestinationOptions,
        opts: &ImageOptions,
    ) -> Result<Self> {
        let temp = util::temp_dir(opts.tmpdir.as_deref(), "skopeo-docker-archive-out-")?;
        let blobs = temp.path().join("blobs");
        fs::create_dir_all(&blobs)?;

        let mut repo_tags = Vec::new();
        let mut push_tag = |r: &DockerReference| {
            let tag = r.familiar_name();
            if !repo_tags.contains(&tag) {
                repo_tags.push(tag);
            }
        };
        if let Some(reference) = reference {
            push_tag(reference);
        } else if let Some(reference) = &dest_opts.source_reference {
            push_tag(reference);
        }
        for tag in &dest_opts.additional_tags {
            push_tag(tag);
        }

        Ok(DockerArchiveDestination {
            path: path.to_path_buf(),
            temp,
            blobs,
            repo_tags,
            manifest: None,
        })
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.blobs.join(digest.dir_file_name())
    }
}

impl ImageDestination for DockerArchiveDestination {
    fn put_blob(
        &mut self,
        digest: &Digest,
        _size: Option<u64>,
        reader: &mut dyn Read,
    ) -> Result<()> {
        let path = self.blob_path(digest);
        let mut temp = tempfile::NamedTempFile::new_in(&self.blobs)?;
        let mut hashing = digest::HashingReader::new(reader);
        std::io::copy(&mut hashing, temp.as_file_mut())?;
        temp.as_file_mut().flush()?;
        if !hashing.verified(digest) {
            bail!(
                "blob {} is corrupt: computed sha256:{}",
                digest,
                hashing.finish_digest()
            );
        }
        temp.persist(&path)
            .map_err(|e| anyhow::anyhow!("writing blob {}: {}", path.display(), e.error))?;
        Ok(())
    }

    fn has_blob(&mut self, digest: &Digest) -> Result<bool> {
        Ok(self.blob_path(digest).exists())
    }

    fn put_manifest(
        &mut self,
        bytes: &[u8],
        media_type: &str,
        instance: Option<&str>,
    ) -> Result<String> {
        if instance.is_some() {
            bail!("docker-archive does not support manifest lists");
        }
        if media_type.contains("manifest.list") || media_type.contains("image.index") {
            bail!("docker-archive does not support manifest lists");
        }
        self.manifest = Some(ManifestBytes {
            bytes: bytes.to_vec(),
            media_type: media_type.to_string(),
        });
        Ok(digest::digest_bytes(bytes))
    }

    fn commit(&mut self) -> Result<()> {
        let manifest_bytes = self
            .manifest
            .take()
            .context("no manifest was written to the docker archive")?;
        let manifest: Manifest =
            serde_json::from_slice(&manifest_bytes.bytes).context("parsing manifest")?;

        let config_digest = Digest::parse(&manifest.config.digest)?;
        let config_path = self.blob_path(&config_digest);
        let config_bytes = fs::read(&config_path)
            .with_context(|| format!("reading config blob {}", config_path.display()))?;

        let mut layer_entries = Vec::new();
        let mut top_layer_hex = String::new();
        for layer in &manifest.layers {
            let digest = Digest::parse(&layer.digest)?;
            let blob_path = self.blob_path(&digest);
            if !blob_path.exists() {
                bail!("layer blob {} is missing", digest);
            }
            let hex = digest.encoded().to_string();
            let layer_name = format!("{hex}/layer.tar");
            layer_entries.push((layer_name, blob_path));
            top_layer_hex = hex;
        }

        let mut tar_temp = tempfile::NamedTempFile::new_in(
            self.path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )
        .with_context(|| format!("creating temporary archive next to {}", self.path.display()))?;
        {
            let mut builder = tar::Builder::new(tar_temp.as_file_mut());
            builder.mode(tar::HeaderMode::Deterministic);

            let save_item = SaveItem {
                config: format!("{}.json", config_digest.encoded()),
                repo_tags: Some(self.repo_tags.clone()),
                layers: Some(
                    layer_entries
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect(),
                ),
                extra: Map::new(),
            };
            let manifest_json = serde_json::to_vec(&vec![save_item])?;
            archive::append_bytes(&mut builder, "manifest.json", &manifest_json)?;
            archive::append_bytes(
                &mut builder,
                &format!("{}.json", config_digest.encoded()),
                &config_bytes,
            )?;

            for (name, blob_path) in &layer_entries {
                let (dir, file_name) = name.split_once('/').unwrap_or((name.as_str(), "layer.tar"));
                let mut dir_header = tar::Header::new_gnu();
                dir_header.set_entry_type(tar::EntryType::Directory);
                dir_header.set_mode(0o755);
                dir_header.set_size(0);
                dir_header.set_mtime(0);
                dir_header.set_cksum();
                let mut dir_name = dir.to_string();
                dir_name.push('/');
                builder.append_data(&mut dir_header, dir_name, std::io::empty())?;

                let layer_temp = self.temp.path().join("layer-tar");
                let (path, size) = materialize_layer(blob_path, &layer_temp)?;
                let mut file = File::open(&path)?;
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_size(size);
                header.set_mtime(0);
                header.set_cksum();
                builder.append_data(&mut header, format!("{dir}/{file_name}"), &mut file)?;
            }

            let repositories = build_repositories(&self.repo_tags, &top_layer_hex);
            archive::append_bytes(
                &mut builder,
                "repositories",
                serde_json::to_string(&repositories)?.as_bytes(),
            )?;
            builder.finish().context("finishing docker archive")?;
        }
        tar_temp.as_file().sync_all().ok();
        tar_temp
            .persist(&self.path)
            .map_err(|e| anyhow::anyhow!("writing {}: {}", self.path.display(), e.error))?;
        Ok(())
    }
}

fn build_repositories(tags: &[String], top_layer_hex: &str) -> Value {
    let mut repositories: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for tag in tags {
        if let Ok(reference) = DockerReference::parse(tag) {
            let repo = docker_save_repo_name(&reference);
            repositories
                .entry(repo)
                .or_default()
                .insert(
                    reference.tag.clone().unwrap_or_else(|| "latest".into()),
                    top_layer_hex.to_string(),
                );
        }
    }
    serde_json::to_value(repositories).unwrap_or(Value::Null)
}

fn docker_save_repo_name(reference: &DockerReference) -> String {
    if reference.registry == "docker.io" {
        reference
            .repository
            .strip_prefix("library/")
            .unwrap_or(&reference.repository)
            .to_string()
    } else {
        format!("{}/{}", reference.registry, reference.repository)
    }
}

/// Copy a (possibly compressed) layer blob into an uncompressed `layer.tar`.
fn materialize_layer(blob_path: &Path, dest: &Path) -> Result<(PathBuf, u64)> {
    let gzip = file_is_gzip(blob_path)?;
    if !gzip {
        let size = fs::metadata(blob_path)?.len();
        return Ok((blob_path.to_path_buf(), size));
    }
    let mut reader = flate2::read::GzDecoder::new(File::open(blob_path)?);
    let mut out = File::create(dest)?;
    let size = std::io::copy(&mut reader, &mut out)?;
    out.flush()?;
    Ok((dest.to_path_buf(), size))
}

fn file_is_gzip(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 2];
    let n = file.read(&mut magic)?;
    if n < 2 {
        return Ok(false);
    }
    if magic == [0x1f, 0x8b] {
        return Ok(true);
    }
    if magic == [0x28, 0xb5] {
        bail!("zstd-compressed layers are not supported by the docker-archive transport");
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(false)
}

fn hash_file(path: &Path) -> Result<(String, u64, bool)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0u8; 2];
    let n = file.read(&mut magic)?;
    let gzip = n == 2 && magic == [0x1f, 0x8b];
    file.seek(SeekFrom::Start(0))?;
    let (encoded, size) = digest::digest_reader(&mut file)?;
    Ok((format!("sha256:{encoded}"), size, gzip))
}
