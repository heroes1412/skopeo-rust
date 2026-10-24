//! `skopeo sync`: docker/dir/yaml sources to docker/dir destinations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::ArgMatches;
use serde::Deserialize;

use crate::cli::{self, GlobalOptions};
use crate::copy::CopyOptions;
use crate::manifest::ManifestFormat;
use crate::options::ImageOptions;
use crate::reference::{DockerReference, ImageReference};
use crate::util;

/// One image scheduled for copying.
struct Planned {
    source: ImageReference,
    /// Display form for logs.
    source_name: String,
    /// Suffix joined onto the destination argument (repo:tag or repo path).
    dest_suffix: String,
    /// Per-registry source options (yaml sources only).
    src_opts: Option<ImageOptions>,
}

/// Go groups refs into `repoDescriptor`s: the ref counter resets per repo and
/// the final "Synced N images from M sources" counts repos as sources.
struct RepoGroup {
    plans: Vec<Planned>,
}

#[derive(Debug, Deserialize, Default)]
struct YamlCredentials {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

#[derive(Debug, Deserialize, Default)]
struct RegistrySyncConfig {
    #[serde(default)]
    images: BTreeMap<String, Vec<String>>,
    #[serde(default, rename = "images-by-tag-regex")]
    images_by_tag_regex: BTreeMap<String, String>,
    #[serde(default, rename = "images-by-semver")]
    images_by_semver: BTreeMap<String, String>,
    #[serde(default)]
    credentials: Option<YamlCredentials>,
    #[serde(default, rename = "tls-verify")]
    tls_verify: Option<bool>,
    #[serde(default, rename = "cert-dir")]
    cert_dir: Option<String>,
}

pub fn run(m: &ArgMatches, global: &GlobalOptions) -> Result<()> {
    let source_transport = m.get_one::<String>("src").map(String::as_str).unwrap_or("");
    let dest_transport = m.get_one::<String>("dest").map(String::as_str).unwrap_or("");
    if source_transport.is_empty() {
        bail!("a source transport must be specified");
    }
    if !matches!(source_transport, "docker" | "dir" | "yaml") {
        bail!("{source_transport:?} is not a valid source transport");
    }
    if dest_transport.is_empty() {
        bail!("a destination transport must be specified");
    }
    if !matches!(dest_transport, "docker" | "dir") {
        bail!("{dest_transport:?} is not a valid destination transport");
    }
    if source_transport == "dir" && dest_transport == "dir" {
        bail!("sync from 'dir' to 'dir' not implemented, consider using rsync instead");
    }

    let mut positional: Vec<String> = Vec::new();
    if let Some(values) = m.get_many::<String>("SOURCE") {
        positional.extend(values.cloned());
    }
    if let Some(values) = m.get_many::<String>("DESTINATION") {
        positional.extend(values.cloned());
    }
    if positional.len() != 2 {
        bail!("exactly two arguments expected");
    }
    let (source_arg, destination_arg) = (&positional[0], &positional[1]);

    if m.get_flag("remove-signatures") && m.get_flag("remove-list-signatures") {
        bail!("only one of --remove-signatures and --remove-list-signatures can be specified");
    }
    for flag in [
        "sign-by",
        "sign-by-sq-fingerprint",
        "sign-by-sigstore",
        "sign-by-sigstore-private-key",
    ] {
        if let Some(value) = m.get_one::<String>(flag).filter(|v| !v.is_empty()) {
            bail!("signing with {flag}={value:?} is not supported by this port");
        }
    }

    let mut src_opts = cli::image_options_from(m, "src-", global, Some("tls-verify"))?;
    let mut dest_opts = cli::image_options_from(m, "dest-", global, Some("tls-verify"))?;
    let (retries, delay) = cli::retry_options(m)?;
    src_opts.retries = retries;
    src_opts.retry_delay = delay;
    dest_opts.retries = retries;
    dest_opts.retry_delay = delay;

    let scoped = m.get_flag("scoped");
    let append_suffix = m.get_one::<String>("append-suffix").cloned().unwrap_or_default();
    let dry_run = m.get_flag("dry-run");
    let keep_going = m.get_flag("keep-going");
    let digest_file = m
        .get_one::<String>("digestfile")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);

    let mut format = None;
    if let Some(value) = m.get_one::<String>("format").filter(|v| !v.is_empty()) {
        format = Some(ManifestFormat::parse(value)?);
    }

    if dry_run {
        util::slog_line("WARN", "Running in dry-run mode", &[]);
    }

    // Discover the images to copy, grouped by source repository.
    let groups: Vec<RepoGroup> = match source_transport {
        "docker" => vec![discover_docker(source_arg, &src_opts)?],
        "dir" => vec![discover_dir(Path::new(source_arg))],
        "yaml" => discover_yaml(Path::new(source_arg), global, &src_opts),
        _ => unreachable!(),
    };
    let sources = groups.len();

    let mut digest_out: Option<std::io::BufWriter<std::fs::File>> = match &digest_file {
        Some(path) if !dry_run => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            Some(std::io::BufWriter::new(
                std::fs::File::create(path)
                    .with_context(|| format!("creating digest file: {:?}", path.display()))?,
            ))
        }
        _ => None,
    };

    let mut errors_present = false;
    let mut copied = 0usize;

    for group in &groups {
        let total = group.plans.len();
        for (index, plan) in group.plans.iter().enumerate() {
            // Destination reference: path.Join(destination, suffix) + suffix-flag.
            let dest_suffix = if scoped {
                plan.dest_suffix.clone()
            } else {
                base_name(&plan.dest_suffix)
            };
            let destination_string = format!(
                "{}{}{}{}",
                destination_arg.trim_end_matches('/'),
                if dest_suffix.starts_with('/')
                    || destination_arg.trim_end_matches('/').is_empty()
                {
                    ""
                } else {
                    "/"
                },
                dest_suffix,
                append_suffix
            );
            let destination_ref = match dest_transport {
                "docker" => ImageReference::Docker(DockerReference::parse(&destination_string)?),
                _ => ImageReference::Dir(PathBuf::from(&destination_string)),
            };

            let from = plan.source_name.clone();
            let to = destination_ref.to_string();
            if dry_run {
                util::slog_line(
                    "INFO",
                    &format!("Would have copied image ref {}/{}", index + 1, total),
                    &[("from", from.clone()), ("to", to.clone())],
                );
                copied += 1;
                continue;
            }
            util::slog_line(
                "INFO",
                &format!("Copying image ref {}/{}", index + 1, total),
                &[("from", from.clone()), ("to", to.clone())],
            );

            // Go refuses to write into any pre-existing per-image directory.
            if let ImageReference::Dir(dest_path) = &destination_ref {
                if dest_path.exists() {
                    bail!(
                        "refusing to overwrite destination directory {:?}",
                        dest_path.display().to_string()
                    );
                }
            }

            let mut copy_opts = CopyOptions::new(
                plan.src_opts.clone().unwrap_or_else(|| src_opts.clone()),
                dest_opts.clone(),
            );
            copy_opts.all = m.get_flag("all");
            copy_opts.format = format;
            copy_opts.remove_signatures = m.get_flag("remove-signatures");
            copy_opts.remove_list_signatures = m.get_flag("remove-list-signatures");
            copy_opts.preserve_digests = m.get_flag("preserve-digests");
            copy_opts.download_foreign_layers = m.get_flag("download-foreign-layers");

            if destination_already_matches(&plan.source, &destination_ref, &copy_opts)? {
                // Go's copy engine reports this on the report stream (stdout).
                println!("Skipping: image already present at destination");
                copied += 1;
                continue;
            }

            match crate::copy::copy_image(&plan.source, &destination_ref, &copy_opts) {
                Ok(digest) => {
                    copied += 1;
                    if let Some(out) = &mut digest_out {
                        use std::io::Write;
                        writeln!(out, "{digest} {destination_ref}")
                            .context("writing digest file")?;
                    }
                }
                Err(err) if keep_going => {
                    errors_present = true;
                    util::slog_line(
                        "ERROR",
                        "Error copying",
                        &[("ref", from.clone()), ("err", format!("{err:#}"))],
                    );
                }
                Err(err) => {
                    return Err(err).context(format!("copying ref {:?}", from));
                }
            }
        }
    }

    if dry_run {
        util::slog_line(
            "INFO",
            &format!("Would have synced {copied} images from {sources} sources"),
            &[],
        );
    } else {
        util::slog_line(
            "INFO",
            &format!("Synced {copied} images from {sources} sources"),
            &[],
        );
    }
    if errors_present {
        bail!("sync failed due to previous reported error(s) for one or more images");
    }
    Ok(())
}

/// `path.Base` for slash-separated image names.
fn base_name(name: &str) -> String {
    let trimmed = name.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((_, base)) => base.to_string(),
        None => trimmed.to_string(),
    }
}

/// `OptimizeDestinationImageAlreadyExists`: skip when the destination already
/// holds the exact same manifest and no transformation would change it.
fn destination_already_matches(
    source: &ImageReference,
    destination: &ImageReference,
    opts: &CopyOptions,
) -> Result<bool> {
    if opts.format.is_some()
        || opts.dest_compress
        || opts.dest_decompress
        || opts.dest_compress_format.is_some()
    {
        return Ok(false);
    }
    let (src_opts, dest_opts) = (&opts.src, &opts.dest);
    let mut src = match crate::transports::open_source(source, src_opts) {
        Ok(src) => src,
        Err(_) => return Ok(false),
    };
    let top = match src.manifest() {
        Ok(top) => top,
        Err(_) => return Ok(false),
    };
    let source_digest = crate::digest::digest_bytes(&top.bytes);
    match destination {
        ImageReference::Docker(dest_ref) => {
            let mut client =
                match crate::registry::RegistryClient::new(dest_ref, dest_opts, "pull") {
                    Ok(client) => client,
                    Err(_) => return Ok(false),
                };
            match client.get_manifest(&dest_ref.reference()) {
                Ok(response) => Ok(response
                    .digest
                    .map(|d| d == source_digest)
                    .unwrap_or_else(|| {
                        crate::digest::digest_bytes(&response.manifest.bytes) == source_digest
                    })),
                Err(_) => Ok(false),
            }
        }
        ImageReference::Dir(path) => {
            let manifest_path = path.join("manifest.json");
            match std::fs::read(&manifest_path) {
                Ok(bytes) => Ok(crate::digest::digest_bytes(&bytes) == source_digest),
                Err(_) => Ok(false),
            }
        }
        _ => Ok(false),
    }
}

fn discover_docker(source_arg: &str, opts: &ImageOptions) -> Result<RepoGroup> {
    // Go parses the source with reference.ParseNormalizedNamed, which
    // rejects transport prefixes.
    if source_arg.contains("://") {
        bail!(
            "obtaining a valid image reference for transport {:?} and reference {:?}: invalid reference format",
            "docker",
            source_arg
        );
    }
    let base = DockerReference::parse(source_arg).with_context(|| {
        format!(
            "obtaining a valid image reference for transport {:?} and reference {:?}",
            "docker", source_arg
        )
    })?;
    let mut plans = Vec::new();
    if base.tag.is_some() || base.digest.is_some() {
        plans.push(Planned {
            source: ImageReference::Docker(base.clone()),
            source_name: format!("docker://{}", base.familiar_name()),
            dest_suffix: base.familiar_name(),
            src_opts: None,
        });
        return Ok(RepoGroup { plans });
    }
    // Tag-less: list all tags in the repository.
    let mut client = crate::registry::RegistryClient::new(&base, opts, "pull")?;
    let tags = client.list_tags()?;
    if tags.is_empty() {
        bail!("no images to sync found in {:?}", source_arg);
    }
    for tag in tags {
        let reference = base.with_tag(tag.clone());
        plans.push(Planned {
            source: ImageReference::Docker(reference.clone()),
            source_name: format!("docker://{}", reference.familiar_name()),
            dest_suffix: reference.familiar_name(),
            src_opts: None,
        });
    }
    Ok(RepoGroup { plans })
}

fn discover_dir(base: &Path) -> RepoGroup {
    let mut plans = Vec::new();
    collect_dir_images(base, base, &mut plans);
    RepoGroup { plans }
}

fn collect_dir_images(root: &Path, current: &Path, plans: &mut Vec<Planned>) {
    if current.join("manifest.json").is_file() {
        let relative = current
            .strip_prefix(root)
            .unwrap_or(Path::new(""))
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        let base_name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        plans.push(Planned {
            source: ImageReference::Dir(current.to_path_buf()),
            source_name: format!("dir:{}", current.display()),
            // Go uses the path relative to the walked base; fall back to the
            // base directory name for a full-path source.
            dest_suffix: if relative.is_empty() {
                base_name
            } else {
                relative
            },
            src_opts: None,
        });
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_dir_images(root, &path, plans);
        }
    }
}

fn discover_yaml(path: &Path, global: &GlobalOptions, cli_src_opts: &ImageOptions) -> Vec<RepoGroup> {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) => {
            util::slog_line("ERROR", &format!("reading {}: {err}", path.display()), &[]);
            return Vec::new();
        }
    };
    let config: BTreeMap<String, RegistrySyncConfig> = match serde_yaml::from_str(&data) {
        Ok(config) => config,
        Err(err) => {
            util::slog_line("ERROR", &format!("parsing {}: {err}", path.display()), &[]);
            return Vec::new();
        }
    };

    let mut groups = Vec::new();
    for (registry, cfg) in config {
        // The CLI source options are the base; YAML values override them.
        let mut src_opts = cli_src_opts.clone();
        src_opts.debug = global.debug;
        src_opts.timeout = global.timeout;
        if let Some(credentials) = &cfg.credentials {
            if !credentials.username.is_empty() {
                src_opts.auth.creds = Some(crate::options::Credentials {
                    username: credentials.username.clone(),
                    password: credentials.password.clone(),
                });
            }
        }
        // Absent tls-verify keeps the CLI setting (Go: OptionalBoolUndefined).
        if let Some(verify) = cfg.tls_verify {
            src_opts.tls_verify = Some(verify);
        }
        if let Some(cert_dir) = &cfg.cert_dir {
            src_opts.cert_dir = Some(PathBuf::from(cert_dir));
        }

        let (registry_host, repository_prefix) = match parse_registry_host(&registry) {
            Ok(parsed) => parsed,
            Err(err) => {
                util::slog_line(
                    "ERROR",
                    "Error parsing registry name, skipping",
                    &[("registry", registry.clone()), ("err", format!("{err:#}"))],
                );
                continue;
            }
        };

        let all_tags = |repo: &str| -> Option<Vec<String>> {
            let full = format!(
                "{registry_host}/{}",
                if repository_prefix.is_empty() {
                    repo.to_string()
                } else {
                    format!("{repository_prefix}/{repo}")
                }
            );
            let base = match DockerReference::parse(&full) {
                Ok(base) => base,
                Err(err) => {
                    util::slog_line(
                        "ERROR",
                        "Error parsing repository name, skipping",
                        &[("repo", full.clone()), ("err", format!("{err:#}"))],
                    );
                    return None;
                }
            };
            match crate::registry::RegistryClient::new(&base, &src_opts, "pull") {
                Ok(mut client) => client.list_tags().ok(),
                Err(err) => {
                    util::slog_line(
                        "ERROR",
                        "Error querying registry, skipping",
                        &[("repo", full), ("err", format!("{err:#}"))],
                    );
                    None
                }
            }
        };

        let mut plans = Vec::new();
        for (repo, tags) in &cfg.images {
            let tags: Vec<String> = if tags.is_empty() {
                match all_tags(repo) {
                    Some(tags) if !tags.is_empty() => tags,
                    _ => continue,
                }
            } else {
                tags.clone()
            };
            for tag in tags {
                // Go tries each entry as a digest first, then a tag.
                push_plan(&mut plans, &registry_host, &repository_prefix, repo, &tag, &src_opts);
            }
        }
        for (repo, pattern) in &cfg.images_by_tag_regex {
            let regex = match regex_lite::Regex::new(pattern) {
                Ok(regex) => regex,
                Err(err) => {
                    util::slog_line(
                        "ERROR",
                        &format!("{err}"),
                        &[("repo", repo.clone()), ("pattern", pattern.clone())],
                    );
                    continue;
                }
            };
            let Some(tags) = all_tags(repo) else { continue };
            for tag in tags {
                if regex.is_match(&tag) {
                    push_plan(&mut plans, &registry_host, &repository_prefix, repo, &tag, &src_opts);
                }
            }
        }
        for (repo, constraint) in &cfg.images_by_semver {
            let req = match parse_semver_constraint(constraint) {
                Ok(req) => req,
                Err(err) => {
                    util::slog_line(
                        "ERROR",
                        &format!("{err}"),
                        &[("repo", repo.clone()), ("constraint", constraint.clone())],
                    );
                    continue;
                }
            };
            let Some(tags) = all_tags(repo) else { continue };
            for tag in tags {
                if let Some(version) = parse_masterminds_version(&tag) {
                    if req.matches(&version) {
                        push_plan(&mut plans, &registry_host, &repository_prefix, repo, &tag, &src_opts);
                    }
                }
            }
        }
        if !plans.is_empty() {
            groups.push(RepoGroup { plans });
        }
    }
    groups
}

/// Split a yaml registry key (`registry.example.com[/namespace]`) into host
/// and repository prefix.
fn parse_registry_host(input: &str) -> Result<(String, String)> {
    let input = input.trim();
    if input.contains("://") {
        bail!("invalid reference format");
    }
    let mut parts = input.splitn(2, '/');
    let host = parts.next().unwrap_or("").to_string();
    if host.is_empty() {
        bail!("empty registry name");
    }
    let prefix = parts.next().unwrap_or("").trim_matches('/').to_string();
    Ok((host, prefix))
}

/// Add one planned image; entries that don't parse as a valid reference are
/// logged and skipped, the way Go's sync does.
fn push_plan(
    plans: &mut Vec<Planned>,
    registry_host: &str,
    prefix: &str,
    repo: &str,
    tag_or_digest: &str,
    src_opts: &ImageOptions,
) {
    let full_repository = if prefix.is_empty() {
        repo.to_string()
    } else {
        format!("{prefix}/{repo}")
    };
    let reference = if tag_or_digest.contains(':') {
        // digest form (e.g. sha256:...)
        DockerReference {
            registry: registry_host.to_string(),
            repository: full_repository,
            tag: None,
            digest: Some(tag_or_digest.to_string()),
        }
    } else {
        DockerReference {
            registry: registry_host.to_string(),
            repository: full_repository,
            tag: Some(tag_or_digest.to_string()),
            digest: None,
        }
    };
    if let Err(err) = reference.validate() {
        util::slog_line(
            "ERROR",
            "Error processing ref, skipping",
            &[
                ("ref", format!("{registry_host}/{repo}:{tag_or_digest}")),
                ("err", format!("{err:#}")),
            ],
        );
        return;
    }
    plans.push(Planned {
        source: ImageReference::Docker(reference.clone()),
        source_name: format!("docker://{}", reference.familiar_name()),
        dest_suffix: reference.familiar_name(),
        src_opts: Some(src_opts.clone()),
    });
}

/// Parse a tag as Masterminds/semver accepts it: optional `v` prefix, one to
/// three numeric components (missing parts are zero).
fn parse_masterminds_version(tag: &str) -> Option<semver::Version> {
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    if tag.is_empty() {
        return None;
    }
    let parts: Vec<&str> = tag.split('.').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut numeric = Vec::new();
    for part in &parts {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        numeric.push(part.parse::<u64>().ok()?);
    }
    while numeric.len() < 3 {
        numeric.push(0);
    }
    Some(semver::Version::new(numeric[0], numeric[1], numeric[2]))
}

/// Masterminds/semver constraint: a bare version means exact equality,
/// everything else goes through the standard requirement parser.
fn parse_semver_constraint(constraint: &str) -> Result<semver::VersionReq> {
    let constraint = constraint.trim();
    let has_operator = constraint
        .chars()
        .any(|c| matches!(c, '>' | '<' | '=' | '~' | '^' | '!' | ',' | '|'));
    if !has_operator {
        let version = parse_masterminds_version(constraint)
            .ok_or_else(|| anyhow::anyhow!("improper constraint: {constraint}"))?;
        return Ok(semver::VersionReq {
            comparators: vec![semver::Comparator {
                op: semver::Op::Exact,
                major: version.major,
                minor: Some(version.minor),
                patch: Some(version.patch),
                pre: version.pre,
            }],
        });
    }
    let cleaned = constraint.replace('v', "");
    semver::VersionReq::parse(&cleaned).map_err(|e| anyhow::anyhow!("{e}"))
}
