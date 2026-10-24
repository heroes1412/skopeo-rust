//! Content digests (`sha256:...`) and streaming verification.

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use sha2::{Digest as _, Sha256, Sha384, Sha512};

/// A `algorithm:encoded` content digest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    algorithm: String,
    encoded: String,
}

impl Digest {
    pub fn parse(s: &str) -> Result<Self> {
        s.parse()
    }

    pub fn sha256(encoded: impl Into<String>) -> Self {
        Digest {
            algorithm: "sha256".to_string(),
            encoded: encoded.into(),
        }
    }

    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    pub fn encoded(&self) -> &str {
        &self.encoded
    }

    pub fn as_str(&self) -> String {
        format!("{}:{}", self.algorithm, self.encoded)
    }

    /// Path component used by the `dir:` transport (`<hex>` for sha256,
    /// `<algorithm>-<hex>` otherwise).
    pub fn dir_file_name(&self) -> String {
        if self.algorithm == "sha256" {
            self.encoded.clone()
        } else {
            format!("{}-{}", self.algorithm, self.encoded)
        }
    }

    /// Validate that a digest is safe to use as a file name component.
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.algorithm.as_str(), "sha256" | "sha384" | "sha512") {
            bail!("unsupported digest algorithm {:?}", self.algorithm);
        }
        let expected = match self.algorithm.as_str() {
            "sha256" => 64,
            "sha384" => 96,
            _ => 128,
        };
        if self.encoded.len() != expected || !self.encoded.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid digest {:?}", self.as_str());
        }
        Ok(())
    }

    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<()> {
        let actual = digest_bytes_with(&self.algorithm, bytes)?;
        if actual != self.encoded {
            bail!(
                "blob digest mismatch: expected {}, got {}:{}",
                self.as_str(),
                self.algorithm,
                actual
            );
        }
        Ok(())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm, self.encoded)
    }
}

impl FromStr for Digest {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (algorithm, encoded) = s
            .split_once(':')
            .with_context(|| format!("invalid digest {s:?}: expected algorithm:encoded"))?;
        if algorithm.is_empty() || encoded.is_empty() {
            bail!("invalid digest {s:?}: expected algorithm:encoded");
        }
        let d = Digest {
            algorithm: algorithm.to_string(),
            encoded: encoded.to_string(),
        };
        d.validate()?;
        Ok(d)
    }
}

pub fn hex_lower(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

pub fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_lower(&Sha256::digest(bytes)))
}

pub fn digest_bytes_with(algorithm: &str, bytes: &[u8]) -> Result<String> {
    match algorithm {
        "sha256" => Ok(hex_lower(&Sha256::digest(bytes))),
        "sha384" => Ok(hex_lower(&Sha384::digest(bytes))),
        "sha512" => Ok(hex_lower(&Sha512::digest(bytes))),
        other => bail!("digest algorithm {other:?} is not available"),
    }
}

pub fn digest_reader<R: Read>(reader: &mut R) -> Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex_lower(&hasher.finalize()), total))
}

/// Reader adapter that hashes bytes as they flow through it.
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    pub fn bytes_read(&self) -> u64 {
        self.bytes
    }

    pub fn finish_digest(&self) -> String {
        hex_lower(&self.hasher.clone().finalize())
    }

    pub fn verified(&self, expected: &Digest) -> bool {
        self.finish_digest() == expected.encoded()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
            self.bytes += n as u64;
        }
        Ok(n)
    }
}

/// Writer adapter that hashes bytes as they flow through them.
pub struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: io::Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    pub fn finish_digest(&self) -> String {
        hex_lower(&self.hasher.clone().finalize())
    }

    pub fn verified(&self, expected: &Digest) -> bool {
        self.finish_digest() == expected.encoded()
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: io::Write> io::Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
            self.bytes += n as u64;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Copy `reader` into `writer`, hashing and verifying the expected digest.
pub fn copy_and_verify<R: Read, W: io::Write>(
    reader: &mut R,
    writer: &mut W,
    expected: &Digest,
) -> Result<u64> {
    let mut hashing = HashingReader::new(reader);
    let written = io::copy(&mut hashing, writer)?;
    if !hashing.verified(expected) {
        bail!(
            "blob {} is corrupt: computed sha256:{}",
            expected,
            hashing.finish_digest()
        );
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats() {
        let d = Digest::parse("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();
        assert_eq!(d.algorithm(), "sha256");
        assert_eq!(
            d.to_string(),
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn rejects_bad_digests() {
        assert!(Digest::parse("sha256:abc").is_err());
        assert!(Digest::parse("no-colon").is_err());
        assert!(Digest::parse("md5:0123").is_err());
    }

    #[test]
    fn computes_known_digest() {
        assert_eq!(
            digest_bytes(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest_bytes(b"hello world"),
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn dir_file_name_matches_containers_image() {
        let d = Digest::sha256("ab");
        assert_eq!(d.dir_file_name(), "ab");
        let d = Digest::parse(
            "sha512:00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();
        assert_eq!(
            d.dir_file_name(),
            "sha512-00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
        );
    }
}
