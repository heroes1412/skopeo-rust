//! The `docker://` transport backed by [`crate::registry::RegistryClient`].

use std::io::{Read, Write as _};
use anyhow::Context as _;

use anyhow::Result;

use crate::digest::Digest;
use crate::manifest::ManifestBytes;
use crate::options::ImageOptions;
use crate::reference::DockerReference;
use crate::registry::RegistryClient;
use crate::transports::{BlobReader, ImageDestination, ImageSource};

pub struct DockerSource {
    client: RegistryClient,
    reference: DockerReference,
}

impl DockerSource {
    pub fn new(reference: &DockerReference, opts: &ImageOptions) -> Result<Self> {
        Ok(DockerSource {
            client: RegistryClient::new(reference, opts, "pull")?,
            reference: reference.clone(),
        })
    }
}

impl ImageSource for DockerSource {
    fn manifest(&mut self) -> Result<ManifestBytes> {
        let reference = self.reference.reference();
        Ok(self.client.get_manifest(&reference)?.manifest)
    }

    fn manifest_for_digest(&mut self, digest: &str) -> Result<ManifestBytes> {
        Ok(self.client.get_manifest(digest)?.manifest)
    }

    fn open_blob(&mut self, digest: &Digest) -> Result<BlobReader> {
        let download = self.client.get_blob(digest)?;
        Ok(BlobReader {
            reader: Box::new(download.reader),
            size: download.size,
        })
    }

    fn tags(&mut self) -> Result<Vec<String>> {
        self.client.list_tags()
    }

    fn docker_reference(&self) -> Option<&DockerReference> {
        Some(&self.reference)
    }
}

pub struct DockerDestination {
    client: RegistryClient,
    reference: DockerReference,
}

impl DockerDestination {
    pub fn new(reference: &DockerReference, opts: &ImageOptions) -> Result<Self> {
        Ok(DockerDestination {
            client: RegistryClient::new(reference, opts, "pull,push")?,
            reference: reference.clone(),
        })
    }
}

impl ImageDestination for DockerDestination {
    fn accepts_foreign_layer_urls(&self) -> bool {
        true
    }

    /// Stream the blob through a hashing spool file first so the declared
    /// digest is verified before anything is uploaded (Go verifies every
    /// blob in the copy engine).
    fn put_blob(
        &mut self,
        digest: &Digest,
        size: Option<u64>,
        reader: &mut dyn Read,
    ) -> Result<()> {
        if self.client.blob_exists(digest)? {
            return Ok(());
        }
        let temp_dir = crate::util::temp_dir(None, "skopeo-upload")?;
        let temp_path = temp_dir.path().join("blob");
        let temp_file = std::fs::File::create(&temp_path)?;
        let mut hashing = crate::digest::HashingWriter::new(temp_file);
        std::io::copy(reader, &mut hashing).context("reading blob")?;
        if !hashing.verified(digest) {
            anyhow::bail!(
                "Digest did not match, expected {}, got sha256:{}",
                digest,
                hashing.finish_digest()
            );
        }
        let actual_size = hashing.bytes_written();
        if let Some(expected) = size {
            if expected != actual_size {
                anyhow::bail!(
                    "blob {digest}: expected {expected} bytes, read {actual_size}"
                );
            }
        }
        hashing.flush()?;
        drop(hashing);
        let mut spooled = std::fs::File::open(&temp_path)?;
        self.client.put_blob(digest, Some(actual_size), &mut spooled)?;
        Ok(())
    }

    fn has_blob(&mut self, digest: &Digest) -> Result<bool> {
        self.client.blob_exists(digest)
    }

    fn put_manifest(
        &mut self,
        bytes: &[u8],
        media_type: &str,
        instance: Option<&str>,
    ) -> Result<String> {
        let reference = match instance {
            Some(digest) => digest.to_string(),
            None => self.reference.reference(),
        };
        self.client.put_manifest(&reference, media_type, bytes)
    }

    fn commit(&mut self) -> Result<()> {
        Ok(())
    }
}
