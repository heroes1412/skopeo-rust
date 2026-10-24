//! Command-line independent options shared by transports, the registry client
//! and the copy engine.

use std::path::PathBuf;
use std::time::Duration;

use crate::manifest::Platform;

/// Credentials supplied directly on the command line.
#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    pub fn parse(spec: &str) -> Self {
        match spec.split_once(':') {
            Some((user, pass)) => Credentials {
                username: user.to_string(),
                password: pass.to_string(),
            },
            None => Credentials {
                username: spec.to_string(),
                password: String::new(),
            },
        }
    }
}

/// Per-image authentication options (`--authfile`, `--creds`, …).
#[derive(Clone, Debug, Default)]
pub struct AuthOptions {
    pub authfile: Option<PathBuf>,
    pub creds: Option<Credentials>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub registry_token: Option<String>,
    pub no_creds: bool,
}

impl AuthOptions {
    /// Merge username/password into `creds`, rejecting contradictory flags.
    pub fn normalized(&self) -> anyhow::Result<Self> {
        let mut out = self.clone();
        if self.creds.is_some() && self.no_creds {
            anyhow::bail!("creds and no-creds cannot be specified at the same time");
        }
        if self.username.is_some() && self.no_creds {
            anyhow::bail!("username and no-creds cannot be specified at the same time");
        }
        if self.creds.is_some() && self.username.is_some() {
            anyhow::bail!("creds and username cannot be specified at the same time");
        }
        if let Some(creds) = &self.creds {
            if creds.username.trim().is_empty() && creds.password.trim().is_empty() {
                anyhow::bail!("credentials can't be empty");
            }
        }
        if self.username.is_some() && self.password.is_none() {
            anyhow::bail!("password must be specified when username is specified");
        }
        if self.password.is_some() && self.username.is_none() {
            anyhow::bail!("username must be specified when password is specified");
        }
        if let (Some(user), Some(pass)) = (&self.username, &self.password) {
            out.creds = Some(Credentials {
                username: user.clone(),
                password: pass.clone(),
            });
        }
        Ok(out)
    }
}

/// Image-level options. Source and destination of a copy have separate
/// instances, just like `containers/image`'s `SystemContext`.
#[derive(Clone, Debug, Default)]
pub struct ImageOptions {
    pub auth: AuthOptions,
    pub tls_verify: Option<bool>,
    pub cert_dir: Option<PathBuf>,
    pub shared_blob_dir: Option<PathBuf>,
    pub debug: bool,
    pub timeout: Option<Duration>,
    pub retries: u32,
    pub retry_delay: Option<Duration>,
    pub user_agent_prefix: Option<String>,
    pub tmpdir: Option<PathBuf>,
    /// Platform used to pick an instance out of a manifest list.
    pub os: String,
    pub arch: String,
    pub variant: Option<String>,
    /// Signature policy. Only used for validation/warnings in this port.
    pub policy: Option<PathBuf>,
    pub insecure_policy: bool,
}

impl ImageOptions {
    pub fn new() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: normalize_arch(std::env::consts::ARCH),
            ..Default::default()
        }
    }

    pub fn platform(&self) -> Platform {
        Platform {
            architecture: self.arch.clone(),
            os: self.os.clone(),
            variant: self.variant.clone(),
            os_version: None,
            os_features: None,
            features: None,
        }
    }

    /// Whether TLS certificates should be validated for this transport.
    pub fn verify_tls(&self) -> bool {
        self.tls_verify.unwrap_or(true)
    }
}

/// Map Rust's architecture names onto OCI platform names.
pub fn normalize_arch(arch: &str) -> String {
    match arch {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        "x86" | "i686" | "i586" => "386".to_string(),
        "arm" | "armv7" => "arm".to_string(),
        other => other.to_string(),
    }
}

/// Something that can be warned about only after parsing a destination.
pub fn warn_debug(enabled: bool, message: impl AsRef<str>) {
    if enabled {
        eprintln!("DEBU[0000] {}", message.as_ref());
    }
}
