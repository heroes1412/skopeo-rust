//! The `oci:` transport: an OCI image layout directory.
//!
//! Layout: `oci-layout`, `index.json` and `blobs/<algorithm>/<encoded>` as
//! defined by the OCI Image Layout specification.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::digest::{self, Digest};
use crate::manifest::{detect_media_type, Descriptor, Index, ManifestBytes, Platform};
use crate::options::ImageOptions;
use crate::reference::DockerReference;
use crate::transports::{BlobReader, ImageDestination, ImageSource};
use crate::util;

const OCI_LAYOUT_VERSION: &str = "1.0.0";
const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const CONTAINERD_NAME_ANNOTATION: &str = "io.containerd.image.name";

pub struct OciSource {
    path: PathBuf,
    reference: String,
    descriptor: Descriptor,
}

impl OciSource {
    pub fn new(path: &Path, reference: &str) -> Result<Self> {
        let layout_path = path.join("oci-layout");
        let layout = fs::read(&layout_path)
            .with_context(|| format!("reading OCI layout {}", layout_path.display()))?;
        let layout: Value = serde_json::from_slice(&layout)
            .with_context(|| format!("parsing {}", layout_path.display()))?;
        let version = layout
            .get("imageLayoutVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        if version.is_empty() {
            bail!("{} is not an OCI image layout", path.display());
        }

        let index_path = path.join("index.json");
        let index_bytes = fs::read(&index_path)
            .with_context(|| format!("reading OCI index {}", index_path.display()))?;
        let index: Index =
            serde_json::from_slice(&index_bytes).context("parsing OCI index.json")?;

        let descriptor = select_descriptor(&index, reference).with_context(|| {
            let available = index
                .manifests
                .iter()
                .filter_map(ref_name)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "locating image {reference:?} in {} (available: {})",
                path.display(),
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available
                }
            )
        })?;

        Ok(OciSource {
            path: path.to_path_buf(),
            reference: reference.to_string(),
            descriptor,
        })
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.path
            .join("blobs")
            .join(digest.algorithm())
            .join(digest.encoded())
    }

    fn read_manifest_blob(&self, digest: &str) -> Result<ManifestBytes> {
        let digest = Digest::parse(digest)?;
        let path = self.blob_path(&digest);
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let media_type = detect_media_type(&bytes, None);
        Ok(ManifestBytes { bytes, media_type })
    }
}

impl ImageSource for OciSource {
    fn manifest(&mut self) -> Result<ManifestBytes> {
        let media_type = if self.descriptor.media_type.is_empty() {
            None
        } else {
            Some(self.descriptor.media_type.clone())
        };
        let mut manifest = self.read_manifest_blob(&self.descriptor.digest)?;
        if let Some(media_type) = media_type {
            if !media_type.is_empty() {
                manifest.media_type = media_type;
            }
        }
        if digest::digest_bytes(&manifest.bytes) != self.descriptor.digest {
            bail!(
                "manifest digest {} does not match index.json entry {}",
                digest::digest_bytes(&manifest.bytes),
                self.descriptor.digest
            );
        }
        Ok(manifest)
    }

    fn manifest_for_digest(&mut self, digest: &str) -> Result<ManifestBytes> {
        self.read_manifest_blob(digest)
    }

    fn open_blob(&mut self, digest: &Digest) -> Result<BlobReader> {
        let path = self.blob_path(digest);
        let file = fs::File::open(&path)
            .with_context(|| format!("opening blob {} at {}", digest, path.display()))?;
        let size = file.metadata().ok().map(|m| m.len());
        Ok(BlobReader {
            reader: Box::new(file),
            size,
        })
    }

    fn tags(&mut self) -> Result<Vec<String>> {
        let index: Index = serde_json::from_slice(&fs::read(self.path.join("index.json"))?)?;
        let mut tags: Vec<String> = index.manifests.iter().filter_map(ref_name).collect();
        tags.sort();
        tags.dedup();
        Ok(tags)
    }

    fn docker_reference(&self) -> Option<&DockerReference> {
        let _ = &self.reference;
        None
    }
}

pub struct OciDestination {
    path: PathBuf,
    tag: String,
    index: Index,
    committed: bool,
}

impl OciDestination {
    pub fn new(path: &Path, reference: &str, _opts: &ImageOptions) -> Result<Self> {
        let index_path = path.join("index.json");
        let (index, created) = if index_path.exists() {
            let bytes = fs::read(&index_path)?;
            let index: Index =
                serde_json::from_slice(&bytes).context("parsing existing OCI index.json")?;
            (index, false)
        } else if path.exists() {
            let mut entries = fs::read_dir(path)?;
            if entries.next().is_some() {
                bail!(
                    "{} is not an OCI image layout, don't want to overwrite important data",
                    path.display()
                );
            }
            (empty_index(), true)
        } else {
            (empty_index(), true)
        };
        if created {
            fs::create_dir_all(path.join("blobs"))
                .with_context(|| format!("creating {}", path.join("blobs").display()))?;
        }
        Ok(OciDestination {
            path: path.to_path_buf(),
            tag: reference.to_string(),
            index,
            committed: false,
        })
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.path
            .join("blobs")
            .join(digest.algorithm())
            .join(digest.encoded())
    }
}

impl ImageDestination for OciDestination {
    fn put_blob(
        &mut self,
        digest: &Digest,
        _size: Option<u64>,
        reader: &mut dyn Read,
    ) -> Result<()> {
        let path = self.blob_path(digest);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let parent = path.parent().unwrap();
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
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
        let digest_string = digest::digest_bytes(bytes);
        let digest = Digest::parse(&digest_string)?;
        let path = self.blob_path(&digest);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            util::atomic_write(&path, bytes)?;
        }
        if instance.is_none() {
            let mut annotations = Map::new();
            annotations.insert(REF_NAME_ANNOTATION.to_string(), Value::String(self.tag.clone()));
            self.index.manifests.retain(|d| {
                d.annotations
                    .as_ref()
                    .and_then(|a| a.get(REF_NAME_ANNOTATION))
                    .map(|name| name != &self.tag)
                    .unwrap_or(true)
            });
            self.index.manifests.push(Descriptor {
                media_type: media_type.to_string(),
                digest: digest_string.clone(),
                size: bytes.len() as i64,
                platform: None,
                urls: None,
                annotations: Some(
                    annotations
                        .into_iter()
                        .map(|(key, value)| {
                            (
                                key,
                                value
                                    .as_str()
                                    .map(str::to_string)
                                    .unwrap_or_else(|| value.to_string()),
                            )
                        })
                        .collect(),
                ),
                extra: Map::new(),
            });
        }
        Ok(digest_string)
    }

    fn commit(&mut self) -> Result<()> {
        if self.committed {
            return Ok(());
        }
        fs::create_dir_all(self.path.join("blobs"))?;
        util::write_json(
            &self.path.join("oci-layout"),
            &json!({"imageLayoutVersion": OCI_LAYOUT_VERSION}),
        )?;
        util::write_json(&self.path.join("index.json"), &self.index)?;
        self.committed = true;
        Ok(())
    }
}

fn empty_index() -> Index {
    Index {
        schema_version: 2,
        media_type: Some(crate::manifest::MEDIA_TYPE_OCI_INDEX.to_string()),
        manifests: Vec::new(),
        annotations: None,
        extra: Map::new(),
    }
}

fn ref_name(descriptor: &Descriptor) -> Option<String> {
    let annotations = descriptor.annotations.as_ref()?;
    if let Some(name) = annotations.get(REF_NAME_ANNOTATION) {
        return Some(name.clone());
    }
    annotations.get(CONTAINERD_NAME_ANNOTATION).map(|name| {
        name.rsplit(':').next().unwrap_or(name).to_string()
    })
}

fn select_descriptor(index: &Index, reference: &str) -> Result<Descriptor> {
    if let Some(found) = index
        .manifests
        .iter()
        .find(|d| ref_name(d).as_deref() == Some(reference))
    {
        return Ok(found.clone());
    }
    if index.manifests.len() == 1 {
        return Ok(index.manifests[0].clone());
    }
    bail!("no image named {reference:?} in index.json")
}

/// Descriptor metadata for tests and `list-tags` on OCI layouts.
pub fn list_refs(path: &Path) -> Result<Vec<(String, String)>> {
    let index: Index = serde_json::from_slice(&fs::read(path.join("index.json"))?)?;
    Ok(index
        .manifests
        .iter()
        .filter_map(|d| ref_name(d).map(|name| (name, d.digest.clone())))
        .collect())
}

/// The platform of an OCI config blob, used when copying single-platform
/// images into an OCI layout.
pub fn platform_from_config(config: &Value) -> Platform {
    Platform {
        architecture: config
            .get("architecture")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        os: config
            .get("os")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        variant: config
            .get("variant")
            .and_then(Value::as_str)
            .map(str::to_string),
        ..Default::default()
    }
}
