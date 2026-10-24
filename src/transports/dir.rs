//! The `dir:` transport.
//!
//! Layout (compatible with `containers/image`):
//!
//! ```text
//! <dir>/manifest.json              top-level manifest
//! <dir>/<hex>.manifest.json        instance manifests (manifest lists)
//! <dir>/<hex>                      blobs (sha256)
//! <dir>/<algorithm>-<hex>          blobs (other algorithms)
//! <dir>/version                    "1.0", "1.1" or "1.2"
//! ```

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::digest::{self, Digest};
use crate::manifest::{detect_media_type, ManifestBytes};
use crate::reference::DockerReference;
use crate::transports::{BlobReader, ImageDestination, ImageSource};

const SUPPORTED_VERSIONS: &[&str] = &["1.0", "1.1", "1.2"];

pub struct DirSource {
    path: PathBuf,
}

impl DirSource {
    pub fn new(path: &Path) -> Result<Self> {
        let source = DirSource {
            path: path.to_path_buf(),
        };
        source.check_version()?;
        if !source.path.join("manifest.json").exists() {
            bail!(
                "{} is not a containers image directory: manifest.json is missing",
                path.display()
            );
        }
        Ok(source)
    }

    fn check_version(&self) -> Result<()> {
        let version_path = self.path.join("version");
        if !version_path.exists() {
            return Ok(());
        }
        let version = fs::read_to_string(&version_path)
            .with_context(|| format!("reading {}", version_path.display()))?;
        let version = version.trim();
        if !SUPPORTED_VERSIONS.contains(&version) {
            bail!(
                "directory {} uses unsupported dir transport version {version:?}",
                self.path.display()
            );
        }
        Ok(())
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.path.join(digest.dir_file_name())
    }

    fn manifest_path(&self, instance: Option<&str>) -> Result<PathBuf> {
        match instance {
            None => Ok(self.path.join("manifest.json")),
            Some(value) => {
                let digest = Digest::parse(value)?;
                Ok(self
                    .path
                    .join(format!("{}.manifest.json", digest.dir_file_name())))
            }
        }
    }
}

impl ImageSource for DirSource {
    fn manifest(&mut self) -> Result<ManifestBytes> {
        let path = self.path.join("manifest.json");
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let media_type = detect_media_type(&bytes, None);
        Ok(ManifestBytes { bytes, media_type })
    }

    fn manifest_for_digest(&mut self, digest: &str) -> Result<ManifestBytes> {
        let path = self.manifest_path(Some(digest))?;
        if !path.exists() {
            bail!(
                "instance manifest {} not found in directory {}",
                digest,
                self.path.display()
            );
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let media_type = detect_media_type(&bytes, None);
        Ok(ManifestBytes { bytes, media_type })
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

    fn docker_reference(&self) -> Option<&DockerReference> {
        None
    }
}

pub struct DirDestination {
    path: PathBuf,
    version: String,
}

impl DirDestination {
    pub fn new(path: &Path) -> Result<Self> {
        if path.exists() {
            if !path.is_dir() {
                bail!("{} exists and is not a directory", path.display());
            }
            let mut entries = fs::read_dir(path)
                .with_context(|| format!("reading directory {}", path.display()))?;
            let non_empty = entries.next().is_some();
            if non_empty && !path.join("version").exists() {
                bail!(
                    "{} is not a containers image directory, don't want to overwrite important data",
                    path.display()
                );
            }
        } else {
            fs::create_dir_all(path)
                .with_context(|| format!("creating directory {}", path.display()))?;
        }
        Ok(DirDestination {
            path: path.to_path_buf(),
            version: "1.1".to_string(),
        })
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.path.join(digest.dir_file_name())
    }
}

impl ImageDestination for DirDestination {
    fn put_blob(
        &mut self,
        digest: &Digest,
        _size: Option<u64>,
        reader: &mut dyn Read,
    ) -> Result<()> {
        let path = self.blob_path(digest);
        let mut temp = tempfile::NamedTempFile::new_in(&self.path)
            .with_context(|| format!("creating temporary blob in {}", self.path.display()))?;
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
        _media_type: &str,
        instance: Option<&str>,
    ) -> Result<String> {
        let path = match instance {
            None => self.path.join("manifest.json"),
            Some(value) => {
                let digest = Digest::parse(value)?;
                self.path.join(format!(
                    "{}.manifest.json",
                    digest.dir_file_name()
                ))
            }
        };
        crate::util::atomic_write(&path, bytes)?;
        if instance.is_some() {
            crate::util::atomic_write(&self.path.join("version"), self.version.as_bytes())?;
        }
        Ok(digest::digest_bytes(bytes))
    }

    fn commit(&mut self) -> Result<()> {
        crate::util::atomic_write(&self.path.join("version"), self.version.as_bytes())?;
        Ok(())
    }
}

/// Read the manifest of a directory just written, for tests and diagnostics.
pub fn read_manifest(path: &Path) -> Result<Vec<u8>> {
    Ok(fs::read(path.join("manifest.json"))?)
}
