//! Image transports.
//!
//! A transport turns an [`ImageReference`] into an [`ImageSource`] (read side)
//! or an [`ImageDestination`] (write side). The copy engine only talks through
//! these traits, exactly like `containers/image` does.

pub mod dir;
pub mod docker;
pub mod docker_archive;
pub mod oci;
pub mod oci_archive;

use std::io::Read;

use anyhow::{bail, Result};

use crate::digest::Digest;
use crate::manifest::ManifestBytes;
use crate::options::ImageOptions;
use crate::reference::{DockerReference, ImageReference};

pub struct BlobReader {
    pub reader: Box<dyn Read + Send>,
    pub size: Option<u64>,
}

pub trait ImageSource {
    /// The image's top-level manifest (or manifest list).
    fn manifest(&mut self) -> Result<ManifestBytes>;

    /// Fetch an instance manifest by digest (children of a manifest list).
    fn manifest_for_digest(&mut self, digest: &str) -> Result<ManifestBytes>;

    fn open_blob(&mut self, digest: &Digest) -> Result<BlobReader>;

    fn read_blob(&mut self, digest: &Digest) -> Result<Vec<u8>> {
        let mut blob = self.open_blob(digest)?;
        let mut data = Vec::new();
        blob.reader.read_to_end(&mut data)?;
        Ok(data)
    }

    /// Tags available in this repository/transport.
    fn tags(&mut self) -> Result<Vec<String>> {
        bail!("listing tags is not supported for this transport")
    }

    fn docker_reference(&self) -> Option<&DockerReference>;
}

pub trait ImageDestination {
    fn put_blob(
        &mut self,
        digest: &Digest,
        size: Option<u64>,
        reader: &mut dyn Read,
    ) -> Result<()>;

    fn has_blob(&mut self, _digest: &Digest) -> Result<bool> {
        Ok(false)
    }

    /// Write a manifest. `instance` is `None` for the top-level manifest and
    /// `Some(digest)` for instances of a manifest list.
    fn put_manifest(
        &mut self,
        bytes: &[u8],
        media_type: &str,
        instance: Option<&str>,
    ) -> Result<String>;

    fn commit(&mut self) -> Result<()>;

    /// Whether the destination can represent multi-architecture images.
    fn supports_multi_arch(&self) -> bool {
        true
    }

    /// Whether the destination can store foreign-layer URLs without the
    /// blob contents (only the docker transport does, like Go).
    fn accepts_foreign_layer_urls(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, Default)]
pub struct DestinationOptions {
    /// Name to record in archives when the destination reference is tag-less.
    pub source_reference: Option<DockerReference>,
    /// Extra tags for `docker-archive:` destinations.
    pub additional_tags: Vec<DockerReference>,
    /// `--dest-compress` / `--dest-decompress` (only `dir`/`docker-archive`
    /// would act on these; this port refuses them explicitly).
    pub compress: Option<bool>,
}

pub fn open_source(reference: &ImageReference, opts: &ImageOptions) -> Result<Box<dyn ImageSource>> {
    match reference {
        ImageReference::Docker(r) => Ok(Box::new(docker::DockerSource::new(r, opts)?)),
        ImageReference::Dir(path) => Ok(Box::new(dir::DirSource::new(path)?)),
        ImageReference::Oci { path, reference } => {
            Ok(Box::new(oci::OciSource::new(path, reference)?))
        }
        ImageReference::OciArchive { path, reference } => Ok(Box::new(
            oci_archive::OciArchiveSource::new(path, reference, opts)?,
        )),
        ImageReference::DockerArchive { path, reference } => Ok(Box::new(
            docker_archive::DockerArchiveSource::new(path, reference.as_ref(), opts)?,
        )),
    }
}

pub fn open_destination(
    reference: &ImageReference,
    opts: &ImageOptions,
    dest_opts: &DestinationOptions,
) -> Result<Box<dyn ImageDestination>> {
    match reference {
        ImageReference::Docker(r) => Ok(Box::new(docker::DockerDestination::new(r, opts)?)),
        ImageReference::Dir(path) => Ok(Box::new(dir::DirDestination::new(path)?)),
        ImageReference::Oci { path, reference } => Ok(Box::new(oci::OciDestination::new(
            path,
            reference,
            opts,
        )?)),
        ImageReference::OciArchive { path, reference } => Ok(Box::new(
            oci_archive::OciArchiveDestination::new(path, reference, opts)?,
        )),
        ImageReference::DockerArchive { path, reference } => Ok(Box::new(
            docker_archive::DockerArchiveDestination::new(
                path,
                reference.as_ref(),
                dest_opts,
                opts,
            )?,
        )),
    }
}
