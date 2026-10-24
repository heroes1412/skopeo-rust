//! Parsing of `transport:details` image names.
//!
//! Supported transports:
//!
//! | name              | syntax                                             |
//! |-------------------|----------------------------------------------------|
//! | `docker://`       | `docker://[registry/]repository[:tag][@digest]`    |
//! | `dir:`            | `dir:/path/to/directory`                            |
//! | `oci:`            | `oci:/path/to/layout[:tag]`                         |
//! | `oci-archive:`    | `oci-archive:/path/to/archive.tar[:tag]`            |
//! | `docker-archive:` | `docker-archive:/path/to/archive.tar[:name:tag]`    |

use std::fmt;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::digest::Digest;

pub const DEFAULT_REGISTRY: &str = "docker.io";
pub const DEFAULT_TAG: &str = "latest";

/// A fully parsed Docker reference, always normalized to include a registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerReference {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl DockerReference {
    /// Parse `[registry/]repository[:tag][@digest]`.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty image name");
        }
        if input.contains(char::is_whitespace) {
            bail!("invalid image name {input:?}: contains whitespace");
        }

        let (name_part, digest) = match input.split_once('@') {
            Some((name, digest)) => {
                let digest = Digest::parse(digest)
                    .with_context(|| format!("invalid digest in image name {input:?}"))?;
                (name, Some(digest.as_str()))
            }
            None => (input, None),
        };

        // A colon after the last slash introduces a tag. Colons before it are
        // registry ports.
        let (name_part, tag) = match name_part.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') && !name.is_empty() => {
                if tag.is_empty() {
                    bail!("invalid image name {input:?}: empty tag");
                }
                (name, Some(tag.to_string()))
            }
            _ => (name_part, None),
        };

        let (registry, repository) = split_registry(name_part)?;
        let repository = normalize_repository(&registry, &repository)?;

        // A name can have either a tag or a digest, not both.
        let tag = if digest.is_some() { None } else { tag };

        Ok(DockerReference {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// Parse a reference that must not contain a tag or digest (used by
    /// `list-tags`).
    pub fn parse_repository(input: &str) -> Result<Self> {
        let mut ref_ = Self::parse(input)?;
        if ref_.tag.is_some() || ref_.digest.is_some() {
            bail!("no tag or digest allowed in reference {input:?}");
        }
        ref_.tag = Some(DEFAULT_TAG.to_string());
        Ok(ref_)
    }

    /// `registry/repository` (what `skopeo inspect` prints as `Name`).
    pub fn name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    /// `registry/repository:tag`, `registry/repository@digest`, or a bare
    /// `registry/repository` for tag-less references (matching how the Go
    /// docker reference prints).
    pub fn familiar_name(&self) -> String {
        if let Some(digest) = &self.digest {
            return format!("{}@{}", self.name(), digest);
        }
        match &self.tag {
            Some(tag) => format!("{}:{}", self.name(), tag),
            None => self.name(),
        }
    }

    /// The reference string to send to the registry API (tag or digest).
    pub fn reference(&self) -> String {
        if let Some(digest) = &self.digest {
            return digest.clone();
        }
        self.tag.clone().unwrap_or_else(|| DEFAULT_TAG.to_string())
    }

    /// Docker Hub serves the API from `registry-1.docker.io` but tokens and
    /// auth files use `docker.io`/`index.docker.io`.
    pub fn api_host(&self) -> String {
        match self.registry.as_str() {
            "docker.io" | "index.docker.io" => "registry-1.docker.io".to_string(),
            other => other.to_string(),
        }
    }

    /// Validate the reference contents (used for references built
    /// programmatically, e.g. by sync discovery).
    pub fn validate(&self) -> Result<()> {
        for component in self.repository.split('/') {
            if component.is_empty()
                || component == "."
                || component == ".."
                || !component
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                bail!("invalid repository {:?}", self.repository);
            }
        }
        if let Some(tag) = &self.tag {
            if tag.is_empty()
                || !tag
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                bail!("invalid tag {tag:?}");
            }
        }
        if let Some(digest) = &self.digest {
            Digest::parse(digest)?;
        }
        Ok(())
    }

    pub fn with_tag(&self, tag: impl Into<String>) -> Self {
        let mut r = self.clone();
        r.tag = Some(tag.into());
        r.digest = None;
        r
    }

    pub fn with_digest(&self, digest: impl Into<String>) -> Self {
        let mut r = self.clone();
        r.digest = Some(digest.into());
        r.tag = None;
        r
    }
}

impl fmt::Display for DockerReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "docker://{}", self.familiar_name())
    }
}

fn split_registry(name: &str) -> Result<(String, String)> {
    let mut parts = name.split('/');
    let first = parts.next().unwrap_or("");
    if first.is_empty() {
        bail!("invalid image name {name:?}: empty repository");
    }
    // A registry component contains a dot or a colon, or is "localhost".
    let looks_like_registry = first.contains('.') || first.contains(':') || first == "localhost";
    if looks_like_registry {
        let rest: Vec<&str> = parts.collect();
        if rest.is_empty() {
            bail!("invalid image name {name:?}: missing repository");
        }
        Ok((first.to_string(), rest.join("/")))
    } else {
        Ok((DEFAULT_REGISTRY.to_string(), name.to_string()))
    }
}

fn normalize_repository(registry: &str, repository: &str) -> Result<String> {
    let repository = repository.trim_matches('/');
    if repository.is_empty() {
        bail!("invalid image name: empty repository");
    }
    for component in repository.split('/') {
        if component.is_empty() {
            bail!("invalid repository {repository:?}: empty path component");
        }
        if component == "." || component == ".." {
            bail!("invalid repository {repository:?}: relative path component");
        }
        if !component
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            bail!("invalid repository {repository:?}: invalid characters");
        }
    }
    if registry == DEFAULT_REGISTRY && !repository.contains('/') {
        Ok(format!("library/{repository}"))
    } else {
        Ok(repository.to_string())
    }
}

/// A parsed `IMAGE-NAME`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageReference {
    Docker(DockerReference),
    Dir(PathBuf),
    Oci { path: PathBuf, reference: String },
    OciArchive { path: PathBuf, reference: String },
    DockerArchive { path: PathBuf, reference: Option<DockerReference> },
}

impl ImageReference {
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        let (transport, rest) = input
            .split_once(':')
            .with_context(|| format!("invalid image name {input:?}: does not specify a transport"))?;
        match transport {
            "docker" => {
                let rest = rest
                    .strip_prefix("//")
                    .with_context(|| format!("docker: image reference {input:?} does not start with docker://"))?;
                if rest.is_empty() {
                    bail!("docker: empty image name");
                }
                Ok(ImageReference::Docker(DockerReference::parse(rest)?))
            }
            "dir" => {
                let path = strip_leading_slashes_except_windows(rest);
                if path.is_empty() {
                    bail!("dir: empty path");
                }
                Ok(ImageReference::Dir(PathBuf::from(path)))
            }
            "oci" => {
                let (path, reference) = split_path_reference(rest, DEFAULT_TAG);
                if path.is_empty() {
                    bail!("oci: empty path");
                }
                Ok(ImageReference::Oci {
                    path: PathBuf::from(path),
                    reference,
                })
            }
            "oci-archive" => {
                let (path, reference) = split_path_reference(rest, DEFAULT_TAG);
                if path.is_empty() {
                    bail!("oci-archive: empty path");
                }
                Ok(ImageReference::OciArchive {
                    path: PathBuf::from(path),
                    reference,
                })
            }
            "docker-archive" => {
                let (path, maybe_ref) = split_archive_path(rest)?;
                if path.is_empty() {
                    bail!("docker-archive: empty path");
                }
                Ok(ImageReference::DockerArchive {
                    path: PathBuf::from(path),
                    reference: maybe_ref,
                })
            }
            other => bail!(
                "unsupported transport {other:?} (supported: docker, dir, oci, oci-archive, docker-archive)"
            ),
        }
    }

    pub fn transport(&self) -> &'static str {
        match self {
            ImageReference::Docker(_) => "docker",
            ImageReference::Dir(_) => "dir",
            ImageReference::Oci { .. } => "oci",
            ImageReference::OciArchive { .. } => "oci-archive",
            ImageReference::DockerArchive { .. } => "docker-archive",
        }
    }

    pub fn docker(&self) -> Option<&DockerReference> {
        match self {
            ImageReference::Docker(r) => Some(r),
            ImageReference::DockerArchive { reference, .. } => reference.as_ref(),
            _ => None,
        }
    }

    /// A human readable name; used for archive layout tags and sync naming.
    pub fn display_name(&self) -> String {
        match self {
            ImageReference::Docker(r) => r.familiar_name(),
            ImageReference::Dir(p) => p.display().to_string(),
            ImageReference::Oci { path, reference } => {
                format!("{}:{}", path.display(), reference)
            }
            ImageReference::OciArchive { path, reference } => {
                format!("{}:{}", path.display(), reference)
            }
            ImageReference::DockerArchive { path, reference } => match reference {
                Some(r) => format!("{}:{}", path.display(), r.familiar_name()),
                None => path.display().to_string(),
            },
        }
    }
}

impl fmt::Display for ImageReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageReference::Docker(r) => write!(f, "docker://{r}")?,
            ImageReference::Dir(p) => write!(f, "dir:{}", p.display())?,
            ImageReference::Oci { path, reference } => {
                write!(f, "oci:{}:{reference}", path.display())?
            }
            ImageReference::OciArchive { path, reference } => {
                write!(f, "oci-archive:{}:{reference}", path.display())?
            }
            ImageReference::DockerArchive { path, reference } => {
                write!(f, "docker-archive:{}", path.display())?;
                if let Some(r) = reference {
                    write!(f, ":{r}")?;
                }
            }
        }
        Ok(())
    }
}

/// `dir://` is accepted by containers/image; keep the path intact while
/// dropping the optional `//`.
fn strip_leading_slashes_except_windows(rest: &str) -> String {
    if rest.starts_with("//") && !is_windows_drive(rest) {
        rest.trim_start_matches("//").to_string()
    } else {
        rest.to_string()
    }
}

fn is_windows_drive(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Split `path:reference` for the OCI transports. On Windows, drive letters
/// and paths with spaces are handled by only treating a suffix as a reference
/// when it cannot be part of a path.
fn split_path_reference(rest: &str, default_reference: &str) -> (String, String) {
    if is_windows_drive(rest) {
        // oci:C:\images\layout:tag
        if let Some(idx) = rest.rfind(':') {
            if idx > 1 {
                let candidate = &rest[idx + 1..];
                if !candidate.is_empty() && !candidate.contains(['/', '\\']) {
                    return (rest[..idx].to_string(), candidate.to_string());
                }
            }
        }
        return (rest.to_string(), default_reference.to_string());
    }
    match rest.rsplit_once(':') {
        Some((path, reference))
            if !path.is_empty()
                && !reference.is_empty()
                && !reference.contains(['/', '\\']) =>
        {
            (path.to_string(), reference.to_string())
        }
        _ => (rest.to_string(), default_reference.to_string()),
    }
}

/// `docker-archive:/path.tar:repo:tag`. The archive suffix is only recognized
/// when it contains a `/`, i.e. it looks like a repository name.
fn split_archive_path(rest: &str) -> Result<(String, Option<DockerReference>)> {
    // On Windows, try the `path:name:tag` split first so that drive letters
    // survive; fall back to treating the whole string as a path.
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() >= 3 {
        // path:name:tag — the last two components form the reference.
        let path = parts[..parts.len() - 2].join(":");
        let candidate = format!("{}:{}", parts[parts.len() - 2], parts[parts.len() - 1]);
        if !path.is_empty() {
            if let Ok(reference) = DockerReference::parse(&candidate) {
                return Ok((path, Some(reference)));
            }
        }
    }
    if parts.len() == 2 && parts[1].contains('/') {
        if let Ok(reference) = DockerReference::parse(parts[1]) {
            return Ok((parts[0].to_string(), Some(reference)));
        }
    }
    Ok((rest.to_string(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_normalization() {
        let r = DockerReference::parse("alpine").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/alpine");
        assert!(r.tag.is_none());
        assert_eq!(r.name(), "docker.io/library/alpine");
        assert_eq!(r.reference(), "latest");
        assert_eq!(r.familiar_name(), "docker.io/library/alpine");

        let r = DockerReference::parse("quay.io/org/app:v1").unwrap();
        assert_eq!(r.registry, "quay.io");
        assert_eq!(r.repository, "org/app");
        assert_eq!(r.familiar_name(), "quay.io/org/app:v1");

        let r = DockerReference::parse("localhost:5000/app").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "app");
        assert!(r.tag.is_none());

        let r = DockerReference::parse("registry.example.com:5000/team/app").unwrap();
        assert_eq!(r.registry, "registry.example.com:5000");
        assert_eq!(r.repository, "team/app");
    }

    #[test]
    fn docker_digest_wins_over_tag() {
        let r = DockerReference::parse(
            "alpine:3.20@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        assert!(r.tag.is_none());
        assert_eq!(
            r.reference(),
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(r.api_host(), "registry-1.docker.io");
    }

    #[test]
    fn transports() {
        assert_eq!(
            ImageReference::parse("dir:/tmp/image").unwrap(),
            ImageReference::Dir(PathBuf::from("/tmp/image"))
        );
        assert_eq!(
            ImageReference::parse("oci:/tmp/layout:v1").unwrap(),
            ImageReference::Oci {
                path: PathBuf::from("/tmp/layout"),
                reference: "v1".into()
            }
        );
        assert_eq!(
            ImageReference::parse("oci:/tmp/layout").unwrap(),
            ImageReference::Oci {
                path: PathBuf::from("/tmp/layout"),
                reference: "latest".into()
            }
        );
        assert!(ImageReference::parse("/tmp/image").is_err());
        assert!(ImageReference::parse("ftp://example.com/image").is_err());
        assert!(ImageReference::parse("docker:alpine").is_err());
    }

    #[test]
    fn list_tags_requires_tagless() {
        assert!(DockerReference::parse_repository("alpine:v1").is_err());
        assert!(DockerReference::parse_repository(
            "alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )
        .is_err());
        DockerReference::parse_repository("alpine").unwrap();
        let r = DockerReference::parse_repository("quay.io/org/app").unwrap();
        assert_eq!(r.repository, "org/app");
    }
}
