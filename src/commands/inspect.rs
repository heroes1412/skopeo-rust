//! `skopeo inspect`.

use anyhow::{bail, Context, Result};
use clap::ArgMatches;

use crate::cli::{self, GlobalOptions};
use crate::commands::write_stdout;
use crate::digest::{self, Digest};
use crate::manifest::{select_platform, ManifestBytes};
use crate::reference::ImageReference;
use crate::registry::HttpStatusError;
use crate::util;

pub fn run(m: &ArgMatches, global: &GlobalOptions) -> Result<()> {
    let opts = cli::image_options_from(m, "", global, None)?;
    let raw = m.get_flag("raw");
    let config = m.get_flag("config");
    let format = m.get_one::<String>("format").map(String::as_str).unwrap_or("");
    let no_tags = m.get_flag("no-tags");
    let digest_algorithm = m
        .get_one::<String>("manifest-digest")
        .map(String::as_str)
        .unwrap_or("");

    let positional: Vec<String> = m
        .get_many::<String>("IMAGE-NAME")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    if positional.len() != 1 {
        bail!("exactly one argument expected");
    }
    let name = &positional[0];

    if raw && !format.is_empty() {
        bail!("raw output does not support format option");
    }

    let reference = ImageReference::parse(name)
        .with_context(|| format!("parsing image name {name:?}"))?;
    let mut src = crate::transports::open_source(&reference, &opts)?;

    // The top-level manifest is what --raw prints and what the reported
    // digest is computed over, exactly like `image.UnparsedInstance(src, nil)`.
    let top = src
        .manifest()
        .with_context(|| format!("retrieving manifest for image {name:?}"))?;

    if raw && !config {
        return write_stdout(&top.bytes);
    }

    // Resolve the instance matching the (overridden) platform for config data.
    let instance: ManifestBytes = if top.is_index() {
        let index = top.parse_index()?;
        let chosen = select_platform(&index, &opts.platform())?;
        src.manifest_for_digest(&chosen.digest)?
    } else {
        top.clone()
    };
    let parsed = instance.parse_manifest()?;
    let config_digest =
        Digest::parse(&parsed.config.digest).context("invalid config digest in manifest")?;
    let config_bytes = src.read_blob(&config_digest)?;
    let image_config = crate::manifest::ImageConfig::parse(&config_bytes)?;

    if config {
        if raw {
            return write_stdout(&config_bytes);
        }
        // Go emits the OCI v1.Image form (fixed field order, 4-space indent).
        let value = oci_image_view(&config_bytes)?;
        return write_stdout(util::go_json_string(&value)?.as_bytes());
    }

    // Digest: explicit algorithm, else the config digest's algorithm.
    let algorithm = if !digest_algorithm.is_empty() {
        // Validate availability the way Go's digest.Algorithm.Available does.
        digest::digest_bytes_with(digest_algorithm, b"")?;
        digest_algorithm.to_string()
    } else {
        config_digest.algorithm().to_string()
    };
    let digest_value = format!(
        "{}:{}",
        algorithm,
        digest::digest_bytes_with(&algorithm, &top.bytes)
            .with_context(|| "computing manifest digest")?
    );

    // RepoTags: docker transport only, graceful on blocked tag listings;
    // raw tag names, like `docker.GetRepositoryTags`.
    let mut repo_tags: Vec<String> = Vec::new();
    if !no_tags {
        if let ImageReference::Docker(_docker_ref) = &reference {
            match src.tags() {
                Ok(tags) => repo_tags = tags,
                Err(err) => {
                    let blocked = err
                        .downcast_ref::<HttpStatusError>()
                        .map(|e| e.status == 403 || e.status == 404)
                        .unwrap_or(false);
                    if blocked {
                        eprintln!("Registry disallows tag list retrieval; skipping");
                    } else {
                        return Err(err).context("determining repository tags");
                    }
                }
            }
        }
    }

    // Go's schema2 Inspect reports a zero time when the config has none;
    // OCI keeps null.
    let created = match image_config.created.clone() {
        Some(value) if !value.is_null() => value,
        _ => {
            if instance.media_type.starts_with("application/vnd.docker.") {
                serde_json::Value::String("0001-01-01T00:00:00Z".to_string())
            } else {
                serde_json::Value::Null
            }
        }
    };

    let mut output = serde_json::Map::new();
    if let Some(docker_ref) = reference.docker() {
        output.insert("Name".into(), serde_json::Value::String(docker_ref.name()));
    }
    // Go leaves Tag empty for schema2/OCI manifests; only legacy schema1 set
    // it, and `omitempty` drops it — never emit a Tag here.
    output.insert("Digest".into(), serde_json::Value::String(digest_value));
    output.insert("RepoTags".into(), serde_json::to_value(&repo_tags)?);
    output.insert("Created".into(), created);
    output.insert(
        "DockerVersion".into(),
        serde_json::Value::String(image_config.docker_version.clone().unwrap_or_default()),
    );
    output.insert(
        "Labels".into(),
        match &image_config.config {
            Some(section) => section
                .labels
                .clone()
                .map(serde_json::to_value)
                .transpose()?
                .unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        },
    );
    output.insert(
        "Architecture".into(),
        serde_json::Value::String(image_config.architecture.clone()),
    );
    output.insert("Os".into(), serde_json::Value::String(image_config.os.clone()));
    // Layers are the manifest's layer digests (not the config diff_ids).
    output.insert(
        "Layers".into(),
        serde_json::to_value(
            parsed
                .layers
                .iter()
                .map(|l| l.digest.clone())
                .collect::<Vec<_>>(),
        )?,
    );
    output.insert(
        "LayersData".into(),
        serde_json::to_value(layers_data(&instance)?)?,
    );
    output.insert(
        "Env".into(),
        match &image_config.config {
            Some(section) => section
                .env
                .clone()
                .map(serde_json::to_value)
                .transpose()?
                .unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        },
    );
    let value = serde_json::Value::Object(output);

    let rendered = render_output(&value, format)?;
    write_stdout(rendered.as_bytes())
}

/// Approximate Go's `img.OCIConfig()`: emit the config as an OCI v1.Image —
/// the well-known keys in OCI struct order, other keys preserved after.
fn oci_image_view(config_bytes: &[u8]) -> Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(config_bytes)?;
    let object = value.as_object().context("image config is not an object")?;
    let order = [
        "created",
        "author",
        "architecture",
        "os",
        "os.version",
        "os.features",
        "variant",
        "config",
        "rootfs",
        "history",
    ];
    let mut out = serde_json::Map::new();
    for key in order {
        if let Some(v) = object.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    for (key, v) in object {
        if !order.contains(&key.as_str()) && key != "container_config" {
            out.insert(key.clone(), v.clone());
        }
    }
    Ok(serde_json::Value::Object(out))
}

/// `LayersData` entries: MIMEType, Digest, Size, Annotations.
fn layers_data(manifest: &ManifestBytes) -> Result<Vec<serde_json::Value>> {
    let parsed = manifest.parse_manifest()?;
    Ok(parsed
        .layers
        .iter()
        .map(|layer| {
            serde_json::json!({
                "MIMEType": layer.media_type,
                "Digest": layer.digest,
                "Size": layer.size,
                "Annotations": layer.annotations.clone().map(|a| serde_json::to_value(a).unwrap_or_default()).unwrap_or(serde_json::Value::Null),
            })
        })
        .collect())
}

/// Render the report. Empty format or a `json` variant emits 4-space-indented
/// JSON; a small subset of Go templates (`.Field` paths and `{{json .}}`) is
/// supported.
fn render_output(value: &serde_json::Value, format: &str) -> Result<String> {
    let trimmed = format.trim();
    if trimmed.is_empty()
        || trimmed == "json"
        || trimmed == "{{json .}}"
        || trimmed == "{{json}}"
        || trimmed == "{{ .json }}"
        || trimmed == "{{.json}}"
    {
        return util::go_json_string(value);
    }
    render_template(value, trimmed)
}

fn render_template(value: &serde_json::Value, template: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            bail!("unsupported format {template:?}: unterminated action");
        };
        let action = after[..end].trim();
        let resolved = if action == "json" || action == "json ." {
            serde_json::to_string(value)?
        } else if let Some(path) = action.strip_prefix("json ") {
            let target = lookup_path(value, path.trim())?;
            serde_json::to_string(target)?
        } else if let Some(path) = action.strip_prefix('.') {
            match lookup_path(value, path.trim()) {
                Ok(serde_json::Value::String(s)) => {
                    out.push_str(s);
                    String::new()
                }
                // Missing fields render empty, matching Go templates over
                // structs whose zero values are empty strings.
                Err(_) | Ok(serde_json::Value::Null) => String::new(),
                Ok(other) => serde_json::to_string(&other)?,
            }
        } else {
            bail!("unsupported format {template:?}: only '.Field' paths and 'json' are supported");
        };
        out.push_str(&resolved);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn lookup_path<'a>(value: &'a serde_json::Value, path: &str) -> Result<&'a serde_json::Value> {
    let mut current = value;
    for component in path.split('.').filter(|c| !c.is_empty()) {
        current = current.get(component).with_context(|| {
            format!("can't evaluate field {component} in type inspect.Output")
        })?;
    }
    Ok(current)
}
