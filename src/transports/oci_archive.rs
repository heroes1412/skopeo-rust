//! The `oci-archive:` transport: a tar archive containing an OCI layout.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::archive;
use crate::digest::Digest;
use crate::manifest::ManifestBytes;
use crate::options::ImageOptions;
use crate::reference::DockerReference;
use crate::transports::oci::{OciDestination, OciSource};
use crate::transports::{BlobReader, ImageDestination, ImageSource};
use crate::util;

pub struct OciArchiveSource {
    _temp: tempfile::TempDir,
    inner: OciSource,
}

impl OciArchiveSource {
    pub fn new(path: &Path, reference: &str, opts: &ImageOptions) -> Result<Self> {
        let temp = util::temp_dir(opts.tmpdir.as_deref(), "skopeo-oci-archive-")?;
        archive::extract_to_dir(path, temp.path())
            .with_context(|| format!("extracting OCI archive {}", path.display()))?;
        let inner = OciSource::new(temp.path(), reference)
            .with_context(|| format!("reading OCI archive {}", path.display()))?;
        Ok(OciArchiveSource { _temp: temp, inner })
    }
}

impl ImageSource for OciArchiveSource {
    fn manifest(&mut self) -> Result<ManifestBytes> {
        self.inner.manifest()
    }

    fn manifest_for_digest(&mut self, digest: &str) -> Result<ManifestBytes> {
        self.inner.manifest_for_digest(digest)
    }

    fn open_blob(&mut self, digest: &Digest) -> Result<BlobReader> {
        self.inner.open_blob(digest)
    }

    fn read_blob(&mut self, digest: &Digest) -> Result<Vec<u8>> {
        self.inner.read_blob(digest)
    }

    fn tags(&mut self) -> Result<Vec<String>> {
        self.inner.tags()
    }

    fn docker_reference(&self) -> Option<&DockerReference> {
        None
    }
}

pub struct OciArchiveDestination {
    path: PathBuf,
    temp: tempfile::TempDir,
    inner: OciDestination,
}

impl OciArchiveDestination {
    pub fn new(path: &Path, reference: &str, opts: &ImageOptions) -> Result<Self> {
        let temp = util::temp_dir(opts.tmpdir.as_deref(), "skopeo-oci-archive-out-")?;
        let inner = OciDestination::new(temp.path(), reference, opts)?;
        Ok(OciArchiveDestination {
            path: path.to_path_buf(),
            temp,
            inner,
        })
    }
}

impl ImageDestination for OciArchiveDestination {
    fn put_blob(
        &mut self,
        digest: &Digest,
        size: Option<u64>,
        reader: &mut dyn Read,
    ) -> Result<()> {
        self.inner.put_blob(digest, size, reader)
    }

    fn has_blob(&mut self, digest: &Digest) -> Result<bool> {
        self.inner.has_blob(digest)
    }

    fn put_manifest(
        &mut self,
        bytes: &[u8],
        media_type: &str,
        instance: Option<&str>,
    ) -> Result<String> {
        self.inner.put_manifest(bytes, media_type, instance)
    }

    fn commit(&mut self) -> Result<()> {
        self.inner.commit()?;
        archive::write_dir_to_tar(self.temp.path(), &self.path)
            .with_context(|| format!("writing OCI archive {}", self.path.display()))?;
        // Keep the directory around until the struct is dropped; it is inside
        // a TempDir so cleanup is automatic.
        let _ = fs::metadata(self.temp.path())?;
        Ok(())
    }
}
