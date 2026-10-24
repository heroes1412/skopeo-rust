//! OCI / Docker image manifest documents, platform selection and format
//! conversion.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MEDIA_TYPE_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MEDIA_TYPE_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MEDIA_TYPE_OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const MEDIA_TYPE_OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";
pub const MEDIA_TYPE_OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const MEDIA_TYPE_OCI_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
pub const MEDIA_TYPE_OCI_NONDISTRIBUTABLE: &str =
    "application/vnd.oci.image.layer.nondistributable.v1.tar";
pub const MEDIA_TYPE_OCI_NONDISTRIBUTABLE_GZIP: &str =
    "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip";

pub const MEDIA_TYPE_DOCKER_INDEX: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const MEDIA_TYPE_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const MEDIA_TYPE_DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
pub const MEDIA_TYPE_DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar";
pub const MEDIA_TYPE_DOCKER_LAYER_GZIP: &str =
    "application/vnd.docker.image.rootfs.diff.tar.gzip";
pub const MEDIA_TYPE_DOCKER_FOREIGN_LAYER: &str =
    "application/vnd.docker.image.rootfs.foreign.diff.tar";
pub const MEDIA_TYPE_DOCKER_FOREIGN_LAYER_GZIP: &str =
    "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip";
pub const MEDIA_TYPE_DOCKER_V2S1: &str = "application/vnd.docker.distribution.manifest.v1+json";
pub const MEDIA_TYPE_DOCKER_V2S1_SIGNED: &str =
    "application/vnd.docker.distribution.manifest.v1+prettyjws";

/// Media types requested from registries, in the order skopeo uses.
pub fn accepted_manifest_types() -> &'static str {
    // Order follows containers/image DefaultRequestedManifestMIMETypes:
    // single manifests first, lists last.
    concat!(
        "application/vnd.oci.image.manifest.v1+json, ",
        "application/vnd.docker.distribution.manifest.v2+json, ",
        "application/vnd.docker.distribution.manifest.v1+prettyjws, ",
        "application/vnd.docker.distribution.manifest.v1+json, ",
        "application/vnd.docker.distribution.manifest.list.v2+json, ",
        "application/vnd.oci.image.index.v1+json"
    )
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Platform {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub architecture: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    #[serde(
        default,
        rename = "os.version",
        skip_serializing_if = "Option::is_none"
    )]
    pub os_version: Option<String>,
    #[serde(
        default,
        rename = "os.features",
        skip_serializing_if = "Option::is_none"
    )]
    pub os_features: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
}

impl Platform {
    pub fn new(os: &str, arch: &str) -> Self {
        Platform {
            architecture: normalize_arch_name(arch),
            os: os.to_string(),
            ..Default::default()
        }
    }

    pub fn display(&self) -> String {
        match &self.variant {
            Some(v) if !v.is_empty() => format!("{}/{}", self.os, self.architecture),
            _ => format!("{}/{}", self.os, self.architecture),
        }
    }

    fn score_against(&self, wanted: &Platform) -> Option<i32> {
        if !self.os.eq_ignore_ascii_case(&wanted.os) {
            return None;
        }
        if normalize_arch_name(&self.architecture) != normalize_arch_name(&wanted.architecture) {
            return None;
        }
        if !variant_matches(
            &normalize_arch_name(&wanted.architecture),
            wanted.variant.as_deref(),
            self.variant.as_deref(),
        ) {
            return None;
        }
        Some(100)
    }
}

/// Variant matching with Go's `WantedPlatforms`/`MatchesPlatform` semantics:
/// the wanted variant expands to a candidate list (itself, weaker ARM-family
/// variants via the compatibility table, and no variant), and the image's
/// variant must be exactly one of them.
fn variant_matches(architecture: &str, wanted: Option<&str>, have: Option<&str>) -> bool {
    let wanted = wanted.filter(|v| !v.is_empty());
    let have = have.filter(|v| !v.is_empty());
    let mut candidates: Vec<Option<String>> = match wanted {
        None => {
            let mut c = vec![None];
            match normalize_arch_name(architecture).as_str() {
                "arm" => c.extend(["v8", "v7", "v6", "v5"].iter().map(|v| Some(v.to_string()))),
                "arm64" => c.push(Some("v8".to_string())),
                _ => {}
            }
            c
        }
        Some(w) => {
            let mut c = vec![Some(w.to_string())];
            let compat: &[&str] = match normalize_arch_name(architecture).as_str() {
                "arm" => &["v8", "v7", "v6", "v5"],
                "arm64" => &["v8"],
                _ => &[],
            };
            if let Some(i) = compat.iter().position(|x| *x == w) {
                c.extend(compat[i + 1..].iter().map(|v| Some(v.to_string())));
            }
            c.push(None);
            c
        }
    };
    candidates.dedup();
    candidates
        .iter()
        .any(|candidate| match (candidate, have) {
            (None, None) => true,
            (Some(wanted), Some(have)) => wanted == have,
            _ => false,
        })
}

pub fn normalize_arch_name(arch: &str) -> String {
    match arch {
        "x86_64" | "x86-64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        "i386" | "i686" | "x86" => "386".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Descriptor {
    pub fn digest(&self) -> Result<crate::digest::Digest> {
        crate::digest::Digest::parse(&self.digest)
            .with_context(|| format!("invalid descriptor digest {:?}", self.digest))
    }

    pub fn is_foreign(&self) -> bool {
        self.media_type.contains("foreign") || self.media_type.contains("nondistributable")
    }

    pub fn is_gzip(&self) -> bool {
        self.media_type.ends_with("+gzip")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType", default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType", default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub manifests: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A manifest returned by a transport.
#[derive(Clone, Debug)]
pub struct ManifestBytes {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl ManifestBytes {
    pub fn digest(&self) -> String {
        crate::digest::digest_bytes(&self.bytes)
    }

    pub fn is_index(&self) -> bool {
        is_index_media_type(&self.media_type)
            || (self.media_type.is_empty() && has_manifests_field(&self.bytes))
    }

    pub fn parse_index(&self) -> Result<Index> {
        serde_json::from_slice(&self.bytes).context("parsing manifest list")
    }

    pub fn parse_manifest(&self) -> Result<Manifest> {
        serde_json::from_slice(&self.bytes).context("parsing image manifest")
    }

    pub fn is_schema1(&self) -> bool {
        // Docker schema1 media types; note that OCI manifests also contain
        // "manifest.v1" in their media type, so a bare substring check would
        // misclassify them.
        self.media_type
            .contains("docker.distribution.manifest.v1")
            || serde_json::from_slice::<Value>(&self.bytes)
                .ok()
                .map(|v| {
                    let version = v.get("schemaVersion").and_then(Value::as_i64);
                    let oci_or_v2_media = v
                        .get("mediaType")
                        .and_then(Value::as_str)
                        .map(|m| !m.is_empty())
                        .unwrap_or(false);
                    version == Some(1) && !oci_or_v2_media
                })
                .unwrap_or(false)
    }
}

pub fn is_index_media_type(media_type: &str) -> bool {
    matches!(media_type, MEDIA_TYPE_OCI_INDEX | MEDIA_TYPE_DOCKER_INDEX)
}

fn has_manifests_field(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .map(|v| v.get("manifests").is_some())
        .unwrap_or(false)
}

fn has_config_field(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .map(|v| v.get("config").is_some())
        .unwrap_or(false)
}

/// Pick the media type of a manifest. `header` is the registry's
/// `Content-Type`; manifests without a `mediaType` field (legal in OCI) are
/// handled by inspecting the JSON structure.
pub fn detect_media_type(bytes: &[u8], header: Option<&str>) -> String {
    if let Some(value) = serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v.get("mediaType").and_then(Value::as_str).map(str::to_string))
    {
        if !value.is_empty() {
            return value;
        }
    }
    if let Some(header) = header {
        let header = header.split(';').next().unwrap_or("").trim();
        if !header.is_empty() && header != "application/json" && header != "text/plain" {
            return header.to_string();
        }
    }
    if has_manifests_field(bytes) {
        MEDIA_TYPE_OCI_INDEX.to_string()
    } else if has_config_field(bytes) {
        MEDIA_TYPE_OCI_MANIFEST.to_string()
    } else {
        String::new()
    }
}

/// Select the instance matching `wanted` from an index.
pub fn select_platform(index: &Index, wanted: &Platform) -> Result<Descriptor> {
    let mut best: Option<(i32, &Descriptor)> = None;
    for descriptor in &index.manifests {
        let Some(platform) = &descriptor.platform else {
            continue;
        };
        if let Some(score) = platform.score_against(wanted) {
            match best {
                Some((best_score, _)) if best_score >= score => {}
                _ => best = Some((score, descriptor)),
            }
        }
    }
    match best {
        Some((_, descriptor)) => Ok(descriptor.clone()),
        None => {
            let available = index
                .manifests
                .iter()
                .filter_map(|d| d.platform.as_ref().map(Platform::display))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "no image found in manifest list for architecture {:?}, variant {:?}, OS {:?} (available: {})",
                wanted.architecture,
                wanted.variant,
                wanted.os,
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available
                }
            )
        }
    }
}

/// Keep only index entries matching one of `platforms`. Used by
/// `--multi-arch linux/amd64,linux/arm64`.
pub fn filter_index(index: &Index, platforms: &[Platform]) -> Result<Index> {
    let mut filtered = index.clone();
    filtered.manifests.retain(|descriptor| {
        descriptor
            .platform
            .as_ref()
            .map(|p| platforms.iter().any(|w| p.score_against(w).is_some()))
            .unwrap_or(false)
    });
    if filtered.manifests.is_empty() {
        bail!("no images in the manifest list matched the requested platforms");
    }
    Ok(filtered)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestFormat {
    Oci,
    V2S2,
    V2S1,
}

impl ManifestFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "oci" => Ok(ManifestFormat::Oci),
            "v2s2" => Ok(ManifestFormat::V2S2),
            "v2s1" => Ok(ManifestFormat::V2S1),
            other => bail!(
                "unknown format {other:?}. Choose one of the supported formats: 'oci', 'v2s1', or 'v2s2'"
            ),
        }
    }

    pub fn media_type(self) -> &'static str {
        match self {
            ManifestFormat::Oci => MEDIA_TYPE_OCI_MANIFEST,
            ManifestFormat::V2S2 => MEDIA_TYPE_DOCKER_MANIFEST,
            ManifestFormat::V2S1 => MEDIA_TYPE_DOCKER_V2S1,
        }
    }
}

fn to_oci_media_type(media_type: &str) -> String {
    match media_type {
        MEDIA_TYPE_DOCKER_INDEX => MEDIA_TYPE_OCI_INDEX,
        MEDIA_TYPE_DOCKER_MANIFEST => MEDIA_TYPE_OCI_MANIFEST,
        MEDIA_TYPE_DOCKER_CONFIG => MEDIA_TYPE_OCI_CONFIG,
        MEDIA_TYPE_DOCKER_LAYER => MEDIA_TYPE_OCI_LAYER,
        MEDIA_TYPE_DOCKER_LAYER_GZIP => MEDIA_TYPE_OCI_LAYER_GZIP,
        MEDIA_TYPE_DOCKER_FOREIGN_LAYER => MEDIA_TYPE_OCI_NONDISTRIBUTABLE,
        MEDIA_TYPE_DOCKER_FOREIGN_LAYER_GZIP => MEDIA_TYPE_OCI_NONDISTRIBUTABLE_GZIP,
        other => other,
    }
    .to_string()
}

fn to_docker_media_type(media_type: &str) -> String {
    match media_type {
        MEDIA_TYPE_OCI_INDEX => MEDIA_TYPE_DOCKER_INDEX,
        MEDIA_TYPE_OCI_MANIFEST => MEDIA_TYPE_DOCKER_MANIFEST,
        MEDIA_TYPE_OCI_CONFIG => MEDIA_TYPE_DOCKER_CONFIG,
        MEDIA_TYPE_OCI_LAYER => MEDIA_TYPE_DOCKER_LAYER,
        MEDIA_TYPE_OCI_LAYER_GZIP => MEDIA_TYPE_DOCKER_LAYER_GZIP,
        MEDIA_TYPE_OCI_NONDISTRIBUTABLE => MEDIA_TYPE_DOCKER_FOREIGN_LAYER,
        MEDIA_TYPE_OCI_NONDISTRIBUTABLE_GZIP => MEDIA_TYPE_DOCKER_FOREIGN_LAYER_GZIP,
        other => other,
    }
    .to_string()
}

/// Convert a manifest (or index) to `format`.
///
/// Returns the original bytes untouched when no conversion is needed so that
/// manifests keep their digest.
pub fn convert_manifest(bytes: &[u8], format: ManifestFormat) -> Result<(Vec<u8>, String)> {
    if format == ManifestFormat::V2S1 {
        bail!("conversion to manifest format v2s1 is not supported by this port");
    }
    let mut value: Value = serde_json::from_slice(bytes).context("parsing manifest")?;
    let source_type = detect_media_type(bytes, None);
    if source_type.contains("docker.distribution.manifest.v1") {
        bail!("converting schema1 manifests is not supported");
    }
    let target_oci = format == ManifestFormat::Oci;
    let source_oci = source_type.starts_with("application/vnd.oci.");
    if !source_type.is_empty() && source_oci == target_oci {
        return Ok((bytes.to_vec(), source_type));
    }

    let map = |mt: &str| -> String {
        if target_oci {
            to_oci_media_type(mt)
        } else {
            to_docker_media_type(mt)
        }
    };

    let object = value
        .as_object_mut()
        .context("manifest is not a JSON object")?;
    if let Some(media_type) = object.get("mediaType").and_then(Value::as_str) {
        let converted = map(media_type);
        object.insert("mediaType".to_string(), Value::String(converted));
    } else {
        let default = if object.contains_key("manifests") {
            if target_oci {
                MEDIA_TYPE_OCI_INDEX
            } else {
                MEDIA_TYPE_DOCKER_INDEX
            }
        } else if target_oci {
            MEDIA_TYPE_OCI_MANIFEST
        } else {
            MEDIA_TYPE_DOCKER_MANIFEST
        };
        object.insert("mediaType".to_string(), Value::String(default.to_string()));
    }

    if let Some(config) = object.get_mut("config").and_then(Value::as_object_mut) {
        if let Some(mt) = config.get("mediaType").and_then(Value::as_str) {
            let converted = map(mt);
            config.insert("mediaType".to_string(), Value::String(converted));
        }
    }
    if let Some(layers) = object.get_mut("layers").and_then(Value::as_array_mut) {
        for layer in layers.iter_mut() {
            if let Some(layer) = layer.as_object_mut() {
                if let Some(mt) = layer.get("mediaType").and_then(Value::as_str) {
                    let converted = map(mt);
                    layer.insert("mediaType".to_string(), Value::String(converted));
                }
            }
        }
    }
    if let Some(manifests) = object.get_mut("manifests").and_then(Value::as_array_mut) {
        for manifest in manifests.iter_mut() {
            if let Some(manifest) = manifest.as_object_mut() {
                if let Some(mt) = manifest.get("mediaType").and_then(Value::as_str) {
                    let converted = map(mt);
                    manifest.insert("mediaType".to_string(), Value::String(converted));
                }
            }
        }
    }

    let out = serde_json::to_vec(&value)?;
    let media_type = detect_media_type(&out, None);
    Ok((out, media_type))
}

/// The subset of the OCI image configuration needed by `inspect`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub created: Option<Value>,
    #[serde(default, rename = "docker_version")]
    pub docker_version: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub config: Option<ConfigSection>,
    #[serde(default)]
    pub rootfs: Option<RootFs>,
    #[serde(default)]
    pub history: Vec<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RootFs {
    #[serde(rename = "type", default)]
    pub fs_type: String,
    #[serde(default, rename = "diff_ids")]
    pub diff_ids: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ConfigSection {
    #[serde(default, rename = "Env")]
    pub env: Option<Vec<String>>,
    #[serde(default, rename = "Labels")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(default, rename = "Entrypoint")]
    pub entrypoint: Option<Value>,
    #[serde(default, rename = "WorkingDir")]
    pub working_dir: Option<String>,
    #[serde(default, rename = "User")]
    pub user: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ImageConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("parsing image configuration")
    }

    pub fn created_string(&self) -> Option<String> {
        match self.created.as_ref()? {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
      "schemaVersion": 2,
      "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
      "config": {
        "mediaType": "application/vnd.docker.container.image.v1+json",
        "size": 10,
        "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "layers": [{
        "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
        "size": 100,
        "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
      }]
    }"#;

    #[test]
    fn detects_media_types() {
        assert_eq!(
            detect_media_type(MANIFEST.as_bytes(), None),
            MEDIA_TYPE_DOCKER_MANIFEST
        );
        assert_eq!(
            detect_media_type(br#"{"schemaVersion":2,"manifests":[]}"#, None),
            MEDIA_TYPE_OCI_INDEX
        );
        assert_eq!(
            detect_media_type(br#"{"schemaVersion":2,"config":{},"layers":[]}"#, None),
            MEDIA_TYPE_OCI_MANIFEST
        );
    }

    #[test]
    fn converts_docker_to_oci_and_back() {
        let (oci, mt) = convert_manifest(MANIFEST.as_bytes(), ManifestFormat::Oci).unwrap();
        assert_eq!(mt, MEDIA_TYPE_OCI_MANIFEST);
        let value: Value = serde_json::from_slice(&oci).unwrap();
        assert_eq!(value["config"]["mediaType"], MEDIA_TYPE_OCI_CONFIG);
        assert_eq!(value["layers"][0]["mediaType"], MEDIA_TYPE_OCI_LAYER_GZIP);

        let (docker, mt) = convert_manifest(&oci, ManifestFormat::V2S2).unwrap();
        assert_eq!(mt, MEDIA_TYPE_DOCKER_MANIFEST);
        let value: Value = serde_json::from_slice(&docker).unwrap();
        assert_eq!(value["config"]["mediaType"], MEDIA_TYPE_DOCKER_CONFIG);
        assert_eq!(
            value["layers"][0]["mediaType"],
            MEDIA_TYPE_DOCKER_LAYER_GZIP
        );
    }

    #[test]
    fn keeps_source_when_format_matches() {
        let (bytes, mt) = convert_manifest(MANIFEST.as_bytes(), ManifestFormat::V2S2).unwrap();
        assert_eq!(bytes, MANIFEST.as_bytes());
        assert_eq!(mt, MEDIA_TYPE_DOCKER_MANIFEST);
    }

    #[test]
    fn selects_platform() {
        let index = Index {
            schema_version: 2,
            media_type: Some(MEDIA_TYPE_OCI_INDEX.into()),
            manifests: vec![
                Descriptor {
                    media_type: MEDIA_TYPE_OCI_MANIFEST.into(),
                    digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    size: 1,
                    platform: Some(Platform {
                        architecture: "amd64".into(),
                        os: "linux".into(),
                        ..Default::default()
                    }),
                    urls: None,
                    annotations: None,
                    extra: Map::new(),
                },
                Descriptor {
                    media_type: MEDIA_TYPE_OCI_MANIFEST.into(),
                    digest: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                    size: 1,
                    platform: Some(Platform {
                        architecture: "arm64".into(),
                        os: "linux".into(),
                        variant: Some("v8".into()),
                        ..Default::default()
                    }),
                    urls: None,
                    annotations: None,
                    extra: Map::new(),
                },
            ],
            annotations: None,
            extra: Map::new(),
        };
        let chosen = select_platform(&index, &Platform::new("linux", "arm64")).unwrap();
        assert!(chosen.digest.contains("bbbb"));
        let chosen = select_platform(&index, &Platform::new("linux", "x86_64")).unwrap();
        assert!(chosen.digest.contains("aaaa"));
        assert!(select_platform(&index, &Platform::new("linux", "s390x")).is_err());
    }
}
