//! The command-line interface, mirroring skopeo's cobra command tree.
//!
//! Flag names, shorthands, defaults and help texts follow
//! `cmd/skopeo/*.go` in the skopeo sources; `Optional*` pflags (which accept
//! `--flag` or `--flag=value`, but never `--flag value`) are modelled with
//! `require_equals(true)` + `default_missing_value`.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Arg, ArgAction, ArgMatches, Command};

use crate::options::ImageOptions;
use crate::util;

/// A pflag "OptionalBool": `--flag` means true, `--flag=false` means false.
fn optional_bool(long: impl Into<String>) -> Arg {
    let long = long.into();
    Arg::new(long.clone())
        .long(long)
        .num_args(0..=1)
        .require_equals(true)
        .default_missing_value("true")
        .value_parser(clap::value_parser!(bool))
}

/// A pflag "OptionalString" (a `pflag.Value` without NoOptDefVal): the value
/// is required, in either `--flag value` or `--flag=value` form; a bare
/// `--flag` is an error, and `--flag=` is set-but-empty.
fn optional_string(long: impl Into<String>) -> Arg {
    let long = long.into();
    Arg::new(long.clone()).long(long)
}

/// A pflag "OptionalInt".
fn optional_int(long: impl Into<String>) -> Arg {
    let long = long.into();
    Arg::new(long.clone())
        .long(long)
        .value_parser(clap::value_parser!(i64))
}

fn authfile_arg() -> Arg {
    Arg::new("authfile")
        .long("authfile")
        .value_name("PATH")
        .default_value(std::env::var("REGISTRY_AUTH_FILE").unwrap_or_default())
        .help("path of the registry credentials file. Default is ${XDG_RUNTIME_DIR}/containers/auth.json")
}

fn retry_args() -> Vec<Arg> {
    vec![
        Arg::new("retry-times")
            .long("retry-times")
            .value_name("TIMES")
            .default_value("0")
            .help("the number of times to possibly retry"),
        Arg::new("retry-delay")
            .long("retry-delay")
            .value_name("DURATION")
            .default_value("0s")
            .help("Fixed delay between retries. If not set, retry uses an exponential backoff delay."),
    ]
}

/// Per-image docker-transport flags. `prefix` is "" or "src-"/"dest-";
/// `creds_alias` adds the hidden legacy alias (`screds`/`dcreds`).
fn docker_image_flags(prefix: &str, creds_alias: Option<&str>, with_authfile: bool) -> Vec<Arg> {
    let mut args = Vec::new();
    if with_authfile {
        args.push(
            optional_string(format!("{prefix}authfile"))
                .help("path of the registry credentials file. Default is ${XDG_RUNTIME_DIR}/containers/auth.json"),
        );
    }
    args.push(
        optional_string(format!("{prefix}creds"))
            .value_name("USERNAME[:PASSWORD]")
            .help("Use `USERNAME[:PASSWORD]` for accessing the registry"),
    );
    if let Some(alias) = creds_alias {
        args.push(
            optional_string(alias.to_string())
                .value_name("USERNAME[:PASSWORD]")
                .hide(true),
        );
    }
    args.push(
        optional_string(format!("{prefix}username"))
            .value_name("USERNAME")
            .help("Username for accessing the registry"),
    );
    args.push(
        optional_string(format!("{prefix}password"))
            .value_name("PASSWORD")
            .help("Password for accessing the registry"),
    );
    args.push(
        optional_string(format!("{prefix}registry-token"))
            .value_name("TOKEN")
            .help("Provide a Bearer token for accessing the registry"),
    );
    args.push(
        Arg::new(format!("{prefix}cert-dir"))
            .long(format!("{prefix}cert-dir"))
            .value_name("PATH")
            .help("use certificates at `PATH` (*.crt, *.cert, *.key) to connect to the registry or daemon"),
    );
    args.push(
        optional_bool(format!("{prefix}tls-verify")).help(
            "require HTTPS and verify certificates when talking to the container registry or daemon",
        ),
    );
    args.push(
        Arg::new(format!("{prefix}no-creds"))
            .long(format!("{prefix}no-creds"))
            .action(ArgAction::SetTrue)
            .help("Access the registry anonymously"),
    );
    args
}

fn shared_blob_and_daemon_args(prefix: &str) -> Vec<Arg> {
    vec![
        Arg::new(format!("{prefix}shared-blob-dir"))
            .long(format!("{prefix}shared-blob-dir"))
            .value_name("DIRECTORY")
            .help("`DIRECTORY` to use to share blobs across OCI repositories"),
        Arg::new(format!("{prefix}daemon-host"))
            .long(format!("{prefix}daemon-host"))
            .value_name("HOST")
            .help("use docker daemon host at `HOST` (docker-daemon: only)"),
    ]
}

/// `--sign-by` family shared by copy and sync.
fn shared_copy_args() -> Vec<Arg> {
    vec![
        Arg::new("remove-signatures")
            .long("remove-signatures")
            .action(ArgAction::SetTrue)
            .help("Do not copy signatures from source"),
        Arg::new("remove-list-signatures")
            .long("remove-list-signatures")
            .action(ArgAction::SetTrue)
            .help("Do not copy manifest list signatures (preserves per-instance signatures)"),
        Arg::new("sign-by")
            .long("sign-by")
            .value_name("FINGERPRINT")
            .help("Sign the image using a GPG key with the specified `FINGERPRINT`"),
        Arg::new("sign-by-sq-fingerprint")
            .long("sign-by-sq-fingerprint")
            .value_name("FINGERPRINT")
            .help("Sign the image using a Sequoia-PGP key with the specified `FINGERPRINT`"),
        Arg::new("sign-by-sigstore")
            .long("sign-by-sigstore")
            .value_name("PATH")
            .help("Sign the image using a sigstore parameter file at `PATH`"),
        Arg::new("sign-by-sigstore-private-key")
            .long("sign-by-sigstore-private-key")
            .value_name("KEY-FILE")
            .help("Sign the image using a sigstore private key at `PATH`"),
        Arg::new("sign-passphrase-file")
            .long("sign-passphrase-file")
            .value_name("PATH")
            .help("Read a passphrase for signing an image from `PATH`"),
        optional_string("format")
            .short('f')
            .value_name("MANIFEST TYPE")
            .help("MANIFEST TYPE (oci, v2s1, or v2s2) to use in the destination (default is manifest type of source, with fallbacks)"),
        Arg::new("preserve-digests")
            .long("preserve-digests")
            .action(ArgAction::SetTrue)
            .help("Preserve digests of images and lists"),
        Arg::new("download-foreign-layers")
            .long("download-foreign-layers")
            .action(ArgAction::SetTrue)
            .help("Download nondistributable (foreign) layers"),
    ]
}

pub fn build_cli() -> Command {
    

    Command::new("skopeo")
        .about("Various operations with container images and container image registries")
        .subcommand_required(false)
        .disable_help_subcommand(false)
        .arg(
            Arg::new("debug")
                .long("debug")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("enable debug output"),
        )
        .arg(
            Arg::new("tls-details")
                .long("tls-details")
                .global(true)
                .value_name("PATH")
                .help("path to a containers-tls-details.yaml(5) file"),
        )
        .arg(
            Arg::new("policy")
                .long("policy")
                .global(true)
                .value_name("PATH")
                .help("Path to a trust policy file"),
        )
        .arg(
            Arg::new("insecure-policy")
                .long("insecure-policy")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("run the tool without any policy check"),
        )
        .arg(
            Arg::new("require-signed")
                .long("require-signed")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("require any pulled image to be signed"),
        )
        .arg(
            Arg::new("registries.d")
                .long("registries.d")
                .global(true)
                .value_name("DIR")
                .help("use registry configuration files in `DIR` (e.g. for container signature storage)"),
        )
        .arg(
            Arg::new("override-arch")
                .long("override-arch")
                .global(true)
                .value_name("ARCH")
                .help("use `ARCH` instead of the architecture of the machine for choosing images"),
        )
        .arg(
            Arg::new("override-os")
                .long("override-os")
                .global(true)
                .value_name("OS")
                .help("use `OS` instead of the running OS for choosing images"),
        )
        .arg(
            Arg::new("override-variant")
                .long("override-variant")
                .global(true)
                .value_name("VARIANT")
                .help("use `VARIANT` instead of the running architecture variant for choosing images"),
        )
        .arg(
            Arg::new("command-timeout")
                .long("command-timeout")
                .global(true)
                .value_name("DURATION")
                .help("timeout for the command execution (e.g. 30s, 10m)"),
        )
        .arg(
            Arg::new("registries-conf")
                .long("registries-conf")
                .global(true)
                .value_name("PATH")
                .hide(true),
        )
        .arg(
            Arg::new("tmpdir")
                .long("tmpdir")
                .global(true)
                .value_name("DIR")
                .help("directory used to store temporary files"),
        )
        .arg(
            Arg::new("user-agent-prefix")
                .long("user-agent-prefix")
                .global(true)
                .value_name("USER-AGENT-PREFIX")
                .help("prefix to add to the user agent string"),
        )
        .arg(
            optional_bool("tls-verify").hide(true).help(
                "Require HTTPS and verify certificates when accessing the registry",
            ),
        )
        .arg(
            Arg::new("version")
                .short('v')
                .long("version")
                .action(ArgAction::SetTrue)
                .help("Version for Skopeo"),
        )
        .subcommands([
            copy_command(),
            delete_command(),
            generate_sigstore_key_command(),
            inspect_command(),
            layers_command(),
            login_command(),
            logout_command(),
            manifest_digest_command(),
            sync_command(),
            standalone_sign_command(),
            standalone_verify_command(),
            list_tags_command(),
            untrusted_dump_command(),
            experimental_image_proxy_command(),
            completion_command(),
        ])
}

fn copy_command() -> Command {
    let mut cmd = Command::new("copy")
        .about("Copy an IMAGE-NAME from one location to another")
        .arg(Arg::new("SOURCE-IMAGE").value_name("SOURCE-IMAGE"))
        .arg(Arg::new("DESTINATION-IMAGE").value_name("DESTINATION-IMAGE"));
    for arg in std::iter::once(authfile_arg())
        .chain(std::iter::once(
            optional_bool("tls-verify").hide(true).help(
                "require HTTPS and verify certificates when accessing the container registry",
            ),
        ))
        .chain(docker_image_flags("src-", Some("screds"), true))
        .chain(shared_blob_and_daemon_args("src-"))
        .chain(docker_image_flags("dest-", Some("dcreds"), true))
        .chain(shared_blob_and_daemon_args("dest-"))
        .chain([
            Arg::new("dest-compress")
                .long("dest-compress")
                .action(ArgAction::SetTrue)
                .help("Compress tarball image layers when saving to directory using the 'dir' transport. (default is same compression type as source)"),
            Arg::new("dest-decompress")
                .long("dest-decompress")
                .action(ArgAction::SetTrue)
                .help("Decompress tarball image layers when saving to directory using the 'dir' transport. (default is same compression type as source)"),
            Arg::new("dest-oci-accept-uncompressed-layers")
                .long("dest-oci-accept-uncompressed-layers")
                .action(ArgAction::SetTrue)
                .help("Allow uncompressed image layers when saving to an OCI image using the 'oci' transport. (default is to compress things that aren't compressed)"),
            Arg::new("dest-compress-format")
                .long("dest-compress-format")
                .value_name("FORMAT")
                .help("`FORMAT` to use for the compression"),
            optional_int("dest-compress-level").value_name("LEVEL").help("`LEVEL` to use for the compression"),
            Arg::new("dest-precompute-digests")
                .long("dest-precompute-digests")
                .action(ArgAction::SetTrue)
                .help("Precompute digests to prevent uploading layers already on the registry using the 'docker' transport."),
            Arg::new("dest-force-compress-format")
                .long("dest-force-compress-format")
                .action(ArgAction::SetTrue)
                .help("Force exclusive use of the compression algorithm set in --dest-compress-format"),
        ])
        .chain(retry_args())
        .chain(shared_copy_args())
        .chain([
            Arg::new("additional-tag")
                .long("additional-tag")
                .value_name("ADDITIONAL-TAG")
                .action(ArgAction::Append)
                .help("additional tags (supports docker-archive)"),
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .action(ArgAction::SetTrue)
                .help("Suppress output information when copying images"),
            Arg::new("all")
                .short('a')
                .long("all")
                .action(ArgAction::SetTrue)
                .help("Copy all images if SOURCE-IMAGE is a list"),
            optional_string("multi-arch").value_name("METHOD").help(
                "How to handle multi-architecture images (system, all, index-only, or comma-separated platform list like linux/amd64,linux/arm64)",
            ),
            Arg::new("sign-identity")
                .long("sign-identity")
                .value_name("REFERENCE")
                .help("Identity of signed image, must be a fully specified docker reference. Defaults to the target docker reference."),
            Arg::new("digestfile")
                .long("digestfile")
                .value_name("PATH")
                .help("Write the digest of the pushed image to the specified file"),
            Arg::new("encryption-key")
                .long("encryption-key")
                .value_name("PROTOCOL:KEY-FILE")
                .action(ArgAction::Append)
                .hide(true)
                .help("*Experimental* key with the encryption protocol to use needed to encrypt the image (e.g. jwe:/path/to/key.pem)"),
            Arg::new("encrypt-layer")
                .long("encrypt-layer")
                .value_name("LAYER-INDEX")
                .action(ArgAction::Append)
                .hide(true)
                .help("*Experimental* the 0-indexed layer indices, with support for negative indexing (e.g. 0 is the first layer, -1 is the last layer)"),
            Arg::new("decryption-key")
                .long("decryption-key")
                .value_name("PROTOCOL:KEY-FILE")
                .action(ArgAction::Append)
                .hide(true)
                .help("*Experimental* key needed to decrypt the image"),
            Arg::new("image-parallel-copies")
                .long("image-parallel-copies")
                .value_name("COUNT")
                .default_value("0")
                .hide(true)
                .help("Maximum number of image layers to be copied (pulled/pushed) simultaneously. Not setting this field will fall back to containers/image defaults."),
            Arg::new("strip-removed-platforms")
                .long("strip-removed-platforms")
                .action(ArgAction::SetTrue)
                .help("Strip removed platforms when copying specific platforms (requires signature removal)"),
        ])
    {
        cmd = cmd.arg(arg);
    }
    cmd
}

fn inspect_command() -> Command {
    let mut cmd = Command::new("inspect")
        .about("Inspect image IMAGE-NAME")
        .arg(Arg::new("IMAGE-NAME").value_name("IMAGE-NAME"));
    for arg in std::iter::once(authfile_arg())
        .chain(docker_image_flags("", None, false))
        .chain(shared_blob_and_daemon_args(""))
        .chain(retry_args())
        .chain([
            Arg::new("raw")
                .long("raw")
                .action(ArgAction::SetTrue)
                .help("output raw manifest or configuration"),
            Arg::new("config")
                .long("config")
                .action(ArgAction::SetTrue)
                .help("output configuration"),
            Arg::new("format")
                .short('f')
                .long("format")
                .value_name("FORMAT")
                .default_value("")
                .help("Format the output to a Go template"),
            Arg::new("no-tags")
                .short('n')
                .long("no-tags")
                .action(ArgAction::SetTrue)
                .help("Do not list the available tags from the repository in the output"),
            Arg::new("manifest-digest")
                .long("manifest-digest")
                .value_name("ALGORITHM")
                .default_value("")
                .help("Algorithm to use for computing manifest digest (sha256, sha512); defaults to algorithm used in config digest"),
        ])
    {
        cmd = cmd.arg(arg);
    }
    cmd
}

fn list_tags_command() -> Command {
    let mut cmd = Command::new("list-tags")
        .about("List tags in the transport/repository specified by the SOURCE-IMAGE")
        .arg(Arg::new("SOURCE-IMAGE").value_name("SOURCE-IMAGE"));
    for arg in std::iter::once(authfile_arg())
        .chain(docker_image_flags("", None, false))
        .chain(retry_args())
    {
        cmd = cmd.arg(arg);
    }
    cmd
}

fn delete_command() -> Command {
    let mut cmd = Command::new("delete")
        .about("Delete image IMAGE-NAME")
        .arg(Arg::new("IMAGE-NAME").value_name("IMAGE-NAME"));
    for arg in std::iter::once(authfile_arg())
        .chain(docker_image_flags("", None, false))
        .chain(shared_blob_and_daemon_args(""))
        .chain(retry_args())
    {
        cmd = cmd.arg(arg);
    }
    cmd
}

fn login_command() -> Command {
    Command::new("login")
        .about("Login to a container registry")
        .arg(Arg::new("REGISTRY").value_name("REGISTRY").num_args(0..=1))
        .args([
            Arg::new("authfile")
                .long("authfile")
                .value_name("PATH")
                .help("path of the authentication file. Use REGISTRY_AUTH_FILE environment variable to override"),
            Arg::new("compat-auth-file")
                .long("compat-auth-file")
                .value_name("PATH")
                .help("path of a Docker-compatible config file to update instead"),
            Arg::new("cert-dir")
                .long("cert-dir")
                .value_name("PATH")
                .help("use certificates at the specified path to access the registry"),
            Arg::new("password")
                .short('p')
                .long("password")
                .value_name("PASSWORD")
                .help("Password for registry"),
            Arg::new("username")
                .short('u')
                .long("username")
                .value_name("USERNAME")
                .help("Username for registry"),
            Arg::new("password-stdin")
                .long("password-stdin")
                .action(ArgAction::SetTrue)
                .help("Take the password from stdin"),
            Arg::new("get-login")
                .long("get-login")
                .action(ArgAction::SetTrue)
                .help("Return the current login user for the registry"),
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::SetTrue)
                .help("Write more detailed information to stdout"),
            optional_bool("tls-verify")
                .help("require HTTPS and verify certificates when accessing the registry"),
        ])
}

fn logout_command() -> Command {
    Command::new("logout")
        .about("Logout of a container registry")
        .arg(Arg::new("REGISTRY").value_name("REGISTRY").num_args(0..=1))
        .args([
            Arg::new("authfile")
                .long("authfile")
                .value_name("PATH")
                .help("path of the authentication file. Use REGISTRY_AUTH_FILE environment variable to override"),
            Arg::new("compat-auth-file")
                .long("compat-auth-file")
                .value_name("PATH")
                .help("path of a Docker-compatible config file to update instead"),
            Arg::new("all")
                .short('a')
                .long("all")
                .action(ArgAction::SetTrue)
                .help("Remove the cached credentials for all registries in the auth file"),
            optional_bool("tls-verify")
                .help("require HTTPS and verify certificates when accessing the registry"),
        ])
}

fn sync_command() -> Command {
    let mut cmd = Command::new("sync")
        .about("Synchronize one or more images from one location to another")
        .arg(Arg::new("SOURCE").value_name("SOURCE"))
        .arg(Arg::new("DESTINATION").value_name("DESTINATION"));
    for arg in std::iter::once(authfile_arg())
        .chain(std::iter::once(
            optional_bool("tls-verify").hide(true).help(
                "require HTTPS and verify certificates when accessing the container registry",
            ),
        ))
        .chain(docker_image_flags("src-", Some("screds"), true))
        .chain(docker_image_flags("dest-", Some("dcreds"), true))
        .chain(retry_args())
        .chain(shared_copy_args())
        .chain([
            Arg::new("src")
                .short('s')
                .long("src")
                .value_name("TRANSPORT")
                .help("SOURCE transport type"),
            Arg::new("dest")
                .short('d')
                .long("dest")
                .value_name("TRANSPORT")
                .help("DESTINATION transport type"),
            Arg::new("scoped")
                .long("scoped")
                .action(ArgAction::SetTrue)
                .help("Images at DESTINATION are prefix using the full source image path as scope"),
            Arg::new("append-suffix")
                .long("append-suffix")
                .value_name("SUFFIX")
                .default_value("")
                .help("String to append to DESTINATION tags"),
            Arg::new("digestfile")
                .long("digestfile")
                .value_name("PATH")
                .help("Write the digests and Image References of the resulting images to the specified file, separated by newlines"),
            Arg::new("all")
                .short('a')
                .long("all")
                .action(ArgAction::SetTrue)
                .help("Copy all images if SOURCE-IMAGE is a list"),
            Arg::new("dry-run")
                .long("dry-run")
                .action(ArgAction::SetTrue)
                .help("Run without actually copying data"),
            Arg::new("keep-going")
                .long("keep-going")
                .action(ArgAction::SetTrue)
                .help("Do not abort the sync if any image copy fails"),
        ])
    {
        cmd = cmd.arg(arg);
    }
    cmd
}

fn manifest_digest_command() -> Command {
    Command::new("manifest-digest")
        .about("Compute a manifest digest of a file")
        .arg(Arg::new("MANIFEST-FILE").value_name("MANIFEST-FILE"))
}

fn standalone_sign_command() -> Command {
    Command::new("standalone-sign")
        .about("Create a signature using local files")
        .arg(Arg::new("MANIFEST").value_name("MANIFEST"))
        .arg(Arg::new("DOCKER-REFERENCE").value_name("DOCKER-REFERENCE"))
        .arg(Arg::new("KEY-FINGERPRINT").value_name("KEY-FINGERPRINT"))
        .args([
            Arg::new("output")
                .short('o')
                .long("output")
                .value_name("SIGNATURE")
                .help("output the signature to `SIGNATURE`"),
            Arg::new("passphrase-file")
                .long("passphrase-file")
                .value_name("PATH")
                .help("file that contains a passphrase for the --sign-by key"),
        ])
}

fn standalone_verify_command() -> Command {
    Command::new("standalone-verify")
        .about("Verify a signature using local files")
        .arg(Arg::new("MANIFEST").value_name("MANIFEST"))
        .arg(Arg::new("DOCKER-REFERENCE").value_name("DOCKER-REFERENCE"))
        .arg(Arg::new("KEY-FINGERPRINTS").value_name("KEY-FINGERPRINTS"))
        .arg(Arg::new("SIGNATURE").value_name("SIGNATURE"))
        .args([Arg::new("public-key-file")
            .long("public-key-file")
            .value_name("PATH")
            .help("File containing public keys. If not specified, will use local GPG keys.")])
}

fn generate_sigstore_key_command() -> Command {
    Command::new("generate-sigstore-key")
        .about("Generate a sigstore public/private key pair")
        .arg(Arg::new("ARGS").value_name("ARGS").num_args(0..).hide(true))
        .args([
            Arg::new("output-prefix")
                .long("output-prefix")
                .value_name("PREFIX")
                .help("Write the keys to `PREFIX`.pub and `PREFIX`.private"),
            Arg::new("passphrase-file")
                .long("passphrase-file")
                .value_name("PATH")
                .help("Read a passphrase for the private key from `PATH`"),
        ])
}

fn layers_command() -> Command {
    let mut cmd = Command::new("layers")
        .hide(true)
        .about("Get layers of IMAGE-NAME")
        .arg(Arg::new("IMAGE-NAME").value_name("IMAGE-NAME"))
        .arg(Arg::new("LAYERS").value_name("LAYER").num_args(0..));
    for arg in std::iter::once(authfile_arg())
        .chain(docker_image_flags("", None, false))
        .chain(shared_blob_and_daemon_args(""))
        .chain(retry_args())
    {
        cmd = cmd.arg(arg);
    }
    cmd
}

fn untrusted_dump_command() -> Command {
    Command::new("untrusted-signature-dump-without-verification")
        .hide(true)
        .about("Dump contents of a signature WITHOUT VERIFYING IT")
        .arg(Arg::new("SIGNATURE").value_name("SIGNATURE"))
}

fn experimental_image_proxy_command() -> Command {
    let mut cmd = Command::new("experimental-image-proxy")
        .hide(true)
        .about("Interactive proxy for fetching container images (EXPERIMENTAL)")
        .args([Arg::new("sockfd")
            .long("sockfd")
            .value_name("SOCKFD")
            .default_value("0")
            .help("Serve on opened socket pair (default 0/stdin)")]);
    for arg in std::iter::once(authfile_arg())
        .chain(docker_image_flags("", None, false))
        .chain(shared_blob_and_daemon_args(""))
    {
        cmd = cmd.arg(arg);
    }
    cmd
}

fn completion_command() -> Command {
    Command::new("completion")
        .hide(true)
        .about("Generate the autocompletion script for the specified shell")
        .subcommands([
            Command::new("bash").arg(Arg::new("no-descriptions").long("no-descriptions").action(ArgAction::SetTrue).help("disable completion descriptions")),
            Command::new("zsh").arg(Arg::new("no-descriptions").long("no-descriptions").action(ArgAction::SetTrue).help("disable completion descriptions")),
            Command::new("fish").arg(Arg::new("no-descriptions").long("no-descriptions").action(ArgAction::SetTrue).help("disable completion descriptions")),
            Command::new("powershell").arg(Arg::new("no-descriptions").long("no-descriptions").action(ArgAction::SetTrue).help("disable completion descriptions")),
        ])
        .subcommand_required(true)
}

// ---------------------------------------------------------------------------
// Parsed global options and per-image options.
// ---------------------------------------------------------------------------

/// Global options shared by every subcommand.
#[derive(Clone, Debug, Default)]
pub struct GlobalOptions {
    pub debug: bool,
    pub override_arch: Option<String>,
    pub override_os: Option<String>,
    pub override_variant: Option<String>,
    pub timeout: Option<Duration>,
    pub tmpdir: Option<std::path::PathBuf>,
    pub user_agent_prefix: Option<String>,
    pub policy: Option<std::path::PathBuf>,
    pub insecure_policy: bool,
    pub require_signed: bool,
    /// The hidden, deprecated root `--tls-verify`.
    pub deprecated_tls_verify: Option<bool>,
}

impl GlobalOptions {
    pub fn from_matches(m: &ArgMatches) -> Result<Self> {
        if m.get_flag("insecure-policy") && m.get_flag("require-signed") {
            bail!("--insecure-policy and --require-signed are mutually exclusive");
        }
        let timeout = match m.get_one::<String>("command-timeout").map(String::as_str) {
            Some(value) if !value.is_empty() => {
                Some(util::parse_duration(value).context("invalid --command-timeout")?)
            }
            _ => None,
        };
        if m.get_one::<bool>("tls-verify").copied().is_some() {
            eprintln!(
                "warning: '--tls-verify' is deprecated, please set this on the specific subcommand"
            );
        }
        Ok(GlobalOptions {
            debug: m.get_flag("debug"),
            override_arch: non_empty(m, "override-arch"),
            override_os: non_empty(m, "override-os"),
            override_variant: non_empty(m, "override-variant"),
            timeout,
            tmpdir: non_empty(m, "tmpdir").map(std::path::PathBuf::from),
            user_agent_prefix: non_empty(m, "user-agent-prefix"),
            policy: non_empty(m, "policy").map(std::path::PathBuf::from),
            insecure_policy: m.get_flag("insecure-policy"),
            require_signed: m.get_flag("require-signed"),
            deprecated_tls_verify: m.get_one::<bool>("tls-verify").copied(),
        })
    }

    /// Base [`ImageOptions`] with platform and global settings applied.
    pub fn base_image_options(&self) -> ImageOptions {
        let mut opts = ImageOptions::new();
        if let Some(arch) = &self.override_arch {
            opts.arch = crate::manifest::normalize_arch_name(arch);
        }
        if let Some(os) = &self.override_os {
            opts.os = normalize_os_name(os);
        }
        if let Some(variant) = &self.override_variant {
            opts.variant = Some(variant.clone());
        }
        opts.debug = self.debug;
        opts.timeout = self.timeout;
        opts.tmpdir = self.tmpdir.clone();
        opts.user_agent_prefix = self.user_agent_prefix.clone();
        opts
    }
}

fn non_empty(m: &ArgMatches, id: &str) -> Option<String> {
    m.get_one::<String>(id)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Map a user-supplied OS name onto OCI platform names.
pub fn normalize_os_name(os: &str) -> String {
    match os {
        "macos" | "osx" => "darwin".to_string(),
        other => other.to_string(),
    }
}

/// Read the per-image docker flags into [`ImageOptions`]. `prefix` is "" for
/// commands with one image and "src-"/"dest-" for copy/sync. `deprecated`
/// names the command-level hidden `--tls-verify`, if any, for the warning.
pub fn image_options_from(
    m: &ArgMatches,
    prefix: &str,
    global: &GlobalOptions,
    deprecated: Option<&str>,
) -> Result<ImageOptions> {
    let mut opts = global.base_image_options();
    let id = |name: &str| format!("{prefix}{name}");

    let mut authfile = non_empty(m, &id("authfile"));
    if authfile.is_none() {
        // Prefixed commands fall back to the shared --authfile flag.
        authfile = non_empty(m, "authfile");
    }
    opts.auth.authfile = authfile.map(std::path::PathBuf::from);

    let creds_value = m
        .get_one::<String>(&id("creds"))
        .cloned()
        .or_else(|| {
            if prefix.is_empty() {
                None
            } else {
                let alias = if prefix == "src-" { "screds" } else { "dcreds" };
                m.get_one::<String>(alias).cloned()
            }
        });
    if let Some(creds) = creds_value {
        opts.auth.creds = Some(crate::options::Credentials::parse(&creds));
    }
    opts.auth.username = m.get_one::<String>(&id("username")).cloned();
    opts.auth.password = m.get_one::<String>(&id("password")).cloned();
    opts.auth.registry_token = non_empty(m, &id("registry-token"));
    opts.auth.no_creds = m.get_flag(&id("no-creds"));
    opts.cert_dir = non_empty(m, &id("cert-dir")).map(std::path::PathBuf::from);
    opts.tls_verify = m
        .get_one::<bool>(&id("tls-verify"))
        .copied()
        .or(global.deprecated_tls_verify)
        .or(match deprecated {
        Some(name) => {
            if let Some(value) = m.get_one::<bool>(name).copied() {
                let instead = match prefix {
                    "src-" => "--src-tls-verify, --dest-tls-verify".to_string(),
                    "dest-" => "--src-tls-verify, --dest-tls-verify".to_string(),
                    _ => "--tls-verify".to_string(),
                };
                eprintln!("warning: '{name}' is deprecated, instead use: {instead}");
                Some(value)
            } else {
                None
            }
        }
        None => None,
    });

    // Retry options are shared, only read for the unprefixed (single) image.
    if prefix.is_empty() {
        opts.retries = m
            .get_one::<String>("retry-times")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let delay = m
            .get_one::<String>("retry-delay")
            .filter(|v| !v.is_empty() && v.as_str() != "0s")
            .map(|v| util::parse_duration(v))
            .transpose()?;
        opts.retry_delay = delay;
    }

    Ok(opts)
}

/// Read the retry options for prefixed (copy/sync) commands.
pub fn retry_options(m: &ArgMatches) -> Result<(u32, Option<Duration>)> {
    let retries = m
        .get_one::<String>("retry-times")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let delay = m
        .get_one::<String>("retry-delay")
        .filter(|v| !v.is_empty() && v.as_str() != "0s")
        .map(|v| util::parse_duration(v))
        .transpose()?;
    Ok((retries, delay))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_flags_accept_bare_and_equals_forms() {
        let cmd = build_cli();
        let matches = cmd
            .try_get_matches_from([
                "skopeo",
                "copy",
                "--dest-tls-verify=false",
                "--src-tls-verify",
                "--dest-creds=a",
                "docker://alpine",
                "dir:/tmp/x",
            ])
            .unwrap();
        let copy = matches.subcommand_matches("copy").unwrap();
        assert_eq!(copy.get_one::<bool>("dest-tls-verify"), Some(&false));
        assert_eq!(copy.get_one::<bool>("src-tls-verify"), Some(&true));
        assert_eq!(copy.get_one::<String>("dest-creds").map(String::as_str), Some("a"));
    }
}
