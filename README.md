# skopeo — skopeo ported to Rust

`skopeo` is a Rust port of [skopeo](https://github.com/podman-container-tools/skopeo),
living in `Rust-Base/` of the skopeo tree. It re-implements the skopeo CLI
(command names, flags, output shapes and exit codes) on top of a small,
dependency-lean Rust core instead of the Go `containers/image` stack.

The headline number: **a stripped release build is ~2.2 MB** (Go skopeo is
~20–25 MB), and it cross-compiles to every target listed below with zero C
dependencies in the default feature set.

The port was audited function-by-function against `cmd/skopeo` and the
vendored `go.podman.io/image` tree: flag names and pflag semantics
(`--flag value`/`--flag=value`, `Optional*` behavior), JSON output field
order and 4-space `MarshalIndent`, inspect `Tag`/`RepoTags`/`Layers`
semantics, delete-by-resolved-digest with scope `*`, the login/logout
message flows, sync discovery/counter/log formats, the simple-signing
payload layout, and the registry `Accept` header order all match upstream.

## Commands

| command | status |
|---|---|
| `copy` | full: platform selection with Go's variant compatibility lists, `--multi-arch` (system/all/index-only/platform list), `--all`, `--format oci/v2s2`, `--digestfile` (digest of the pushed manifest), `--additional-tag`, `--dest-compress*` (dir: destination, gzip, `zstd` behind a feature flag), `--dest-decompress`, `--dest-oci-accept-uncompressed-layers`, ineffective-flag warnings, retry flags, verified blob uploads |
| `inspect` | full: `--raw`, `--config` (OCI v1.Image view), `--no-tags`, `--manifest-digest sha256/sha384/sha512`, JSON byte-shape matches `inspect.Output` (4-space indent, no `Tag` for schema2/OCI, raw `RepoTags`, `Layers` = manifest digests); `--format` supports `json` and a `.Field`/`{{json .}}` template subset |
| `list-tags` | docker + docker-archive transports, tag-less reference enforcement, 4-space output |
| `delete` | resolves the manifest then deletes by digest (scope `*`), like upstream |
| `login` / `logout` | full: token auth flow, stored-credentials messages ("Authenticating with existing credentials…", "Existing credentials are valid/invalid…"), `--password-stdin` rules, identity-token guard, `--get-login`, `--all`, repository-scoped keys, `--compat-auth-file` (Docker key spelling), `--cert-dir` |
| `sync` | full: `docker`/`dir`/`yaml` sources (digest refs, `images-by-tag-regex`, `images-by-semver` with Masterminds-style `v`/partial versions, `credentials`, `tls-verify` override, `cert-dir`), `docker`/`dir` destinations, per-repo ref counters, `--scoped`, `--append-suffix`, `--dry-run`, `--keep-going`, `--digestfile`, `refusing to overwrite destination directory`, destination-already-exists skip |
| `manifest-digest` | full |
| `standalone-sign` / `standalone-verify` | same simple-signing payload (sorted-key JSON, `atomic` creator) via the `gpg` binary |
| `untrusted-signature-dump-without-verification` | full (hidden) |
| `layers` | full (hidden, deprecated): config last, `sha256:` auto-prefix |
| `generate-sigstore-key` | CLI surface only; returns a clear error (use `cosign generate-key-pair`) |
| `completion bash/zsh/fish/powershell` | full (hidden, subcommand form) |
| `experimental-image-proxy` | errors out (hidden) |

Transports: `docker://`, `dir:`, `oci:`, `oci-archive:`, `docker-archive:`
(docker-archive layers are stored uncompressed, like `docker save`).
`containers-storage:` and `docker-daemon:` are not ported (they need the Go
containers/storage graph-driver stack).

Exit codes match skopeo: `0` success, `1` error, `2` image-not-found
(registry 404 / manifest unknown). Errors print as `Error: <message>`;
`sync` logs use the slog format (`INFO/WARN/ERROR …`, timestamped when
stderr is not a TTY — in UTC rather than local time).

## Building

See **[BUILDING.md](BUILDING.md)** for the full environment setup guide
(Windows + RHEL: what to install, download links, which script to run).

Quick start: `build.bat` (cmd) / `build.ps1` (PowerShell) / `./build.sh`
(bash) build the default matrix (windows-amd64, linux-amd64, linux-arm64)
into `dist/`.

```sh
cargo build --release          # ~2.2 MB stripped binary in target/release/
cargo test                     # unit + CLI tests
```

Release profile (already in `Cargo.toml`): `opt-level = "z"`, fat LTO,
`codegen-units = 1`, `panic = "abort"`, `strip = true`.

Optional features:

- `zstd` — adds zstd decompression/recompression for `--dest-compress-format zstd`
  and zstd layers (adds ~1 MB; default off).

## Multi-arch builds

Fully static Linux binaries (musl), plus macOS and Windows:

| target | host | tool |
|---|---|---|
| `x86_64-unknown-linux-musl` | any | `cargo zigbuild` |
| `aarch64-unknown-linux-musl` | any | `cargo zigbuild` |
| `armv7-unknown-linux-musleabihf` | any | `cargo zigbuild` |
| `riscv64gc-unknown-linux-musl` | any | `cargo zigbuild` |
| `aarch64-apple-darwin` | macOS | `cargo build` |
| `x86_64-apple-darwin` | macOS | `cargo build` |
| `x86_64-pc-windows-msvc` | Windows | `cargo build` |
| `aarch64-pc-windows-msvc` | any | `cargo xwin` |

Verified build results (cargo-zigbuild 0.24 + zig 0.14, Windows host):

| artifact | size |
|---|---|
| `dist/skopeo-x86_64-pc-windows-msvc.exe` | 2.20 MB |
| `dist/skopeo-x86_64-linux-musl` | 2.58 MB — ELF x86-64, statically linked, stripped |
| `dist/skopeo-aarch64-linux-musl` | 2.20 MB — ELF aarch64, statically linked, stripped |

```powershell
.uild.ps1                     # default matrix: windows-amd64, linux-amd64, linux-arm64
.uild.ps1 -Targets x86_64-unknown-linux-musl
```

```sh
./build.sh                      # bash equivalent (adds macOS targets on macOS hosts)
./build.sh x86_64-unknown-linux-musl aarch64-unknown-linux-musl
```

`.github/workflows/release.yml` builds the full matrix on tags and attaches
all binaries to the GitHub release. TLS is rustls + webpki-roots (no OpenSSL),
so every target builds without system headers.

## Auth & configuration

- Credentials: `--authfile` (containers `auth.json`, written tab-indented
  like Go; Docker `config.json` spellings tolerated; repository-namespace
  keys walked longest-first), `$REGISTRY_AUTH_FILE`,
  `${XDG_RUNTIME_DIR}/containers/auth.json`, `~/.docker/config.json`,
  credential helpers (`docker-credential-*`, `<token>` → identity token).
- TLS: verified by default; `--tls-verify=false` or `--src-tls-verify` /
  `--dest-tls-verify`; CA bundles via `--cert-dir` (`*.crt`).
- Global flags: `--debug`, `--command-timeout`, `--override-arch/os/variant`,
  `--tmpdir`, `--user-agent-prefix`, `--policy`/`--insecure-policy`
  (accepted; signatures are not verified in this port — see Divergences).

## Divergences from Go skopeo

1. **Signature verification/policy** is not implemented; `--policy`,
   `--insecure-policy`, `--require-signed` are parsed but inert, signatures
   are never copied to destinations (`--remove-signatures` is a no-op), and
   `--sign-by*` flags error out. `standalone-sign`/`standalone-verify` shell
   out to `gpg` and produce/consume the same blob format.
2. **`generate-sigstore-key`** is not implemented (see table above).
3. **`containers-storage:` / `docker-daemon:`** transports and
   `registries.conf` mirroring are out of scope.
4. **schema1 manifests** are detected and refused; Go 5.42 still converts
   them. `--format v2s1` is accepted at parse time and errors at conversion.
5. **`--image-parallel-copies`**: copies are sequential (the flag is accepted
   and ignored). Cross-repo blob mounts (`?mount=&from=`) are not attempted;
   blob existence is checked with HEAD before every upload.
6. **`--format` templates** beyond `json`/`.Field`/`{{json .}}` are not
   evaluated; `completion --no-descriptions` is accepted and ignored.
7. Progress: the non-TTY line format matches, but Go's TTY mode renders
   animated progress bars; this port always prints plain lines.
8. `sync` timestamps in non-TTY log lines are UTC instead of local time.
9. `sync` to a `dir:` destination creates directories named `<image>:<tag>`
   (same as Go); on Windows the `:` makes those directory names invalid.
10. `logout` removes all spelling variants of the given key (Go removes
    only the exact key).

## Layout

```
src/
  cli.rs         clap command tree mirroring the cobra spec
  commands/      one module per subcommand
  registry.rs    Docker Registry HTTP API v2 client (bearer auth, pagination)
  copy.rs        transport-agnostic copy engine (platform selection,
                 manifest conversion, gzip/zstd transcode)
  transports/    docker, dir, oci, oci-archive, docker-archive
  auth.rs        auth.json handling + credential helpers
  manifest.rs    manifest/index documents, platform selection
  reference.rs   transport:details name parsing
  digest.rs      sha256/sha384/sha512 digests + streaming verification
  archive.rs     tar helpers
```
