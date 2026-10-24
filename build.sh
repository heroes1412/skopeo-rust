#!/usr/bin/env bash
# Build skopeo for every supported target.
#
# Prerequisites:
#   rustup target add <target>   (see TARGETS below)
#   pip install cargo-zigbuild   (or: cargo install cargo-zigbuild)
#     - linux targets are cross-compiled with cargo-zigbuild, which needs zig
#     - macOS targets build natively on a macOS host
#     - Windows targets: x86_64 builds natively on Windows; aarch64 uses
#       cargo-xwin (`cargo install cargo-xwin`)
#
# Usage: ./build.sh [--out DIR] [target ...]

set -euo pipefail
cd "$(dirname "$0")"

OUT="${OUT:-dist}"

# Embed the git commit into the binary (skopeo --version).
if [ -z "${SKOPEO_COMMIT:-}" ]; then
    SKOPEO_COMMIT="$(git -C "$(dirname "$0")" rev-parse --short HEAD 2>/dev/null || echo unknown)"
    export SKOPEO_COMMIT
fi
mkdir -p "$OUT"

LINUX_TARGETS=(
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
    armv7-unknown-linux-musleabihf
    riscv64gc-unknown-linux-musl
)
DEFAULT_TARGETS=()

case "$(uname -s)" in
    Linux*)  DEFAULT_TARGETS=("${LINUX_TARGETS[@]}") ;;
    Darwin*) DEFAULT_TARGETS=(aarch64-apple-darwin x86_64-apple-darwin) ;;
    MINGW*|MSYS*|CYGWIN*) DEFAULT_TARGETS=(x86_64-pc-windows-msvc) ;;
esac

TARGETS=("${@:-${DEFAULT_TARGETS[@]}}")

for target in "${TARGETS[@]}"; do
    echo "==> building $target"
    case "$target" in
        *-windows-msvc)
            if [[ "$target" == aarch64-pc-windows-msvc && "$(rustc -vV | awk '/host:/ {print $2}')" != aarch64-pc-windows-msvc ]]; then
                cargo xwin build --release --target "$target"
            else
                cargo build --release --target "$target"
            fi
            binary="target/$target/release/skopeo.exe"
            name="skopeo-$target.exe"
            ;;
        *-apple-darwin)
            cargo build --release --target "$target"
            binary="target/$target/release/skopeo"
            name="skopeo-$target"
            ;;
        *musl*|*gnu*)
            cargo zigbuild --release --target "$target"
            binary="target/$target/release/skopeo"
            name="skopeo-$target"
            ;;
        *)
            echo "unknown target $target" >&2
            exit 1
            ;;
    esac
    cp "$binary" "$OUT/$name"
    if command -v strip >/dev/null 2>&1; then
        strip "$OUT/$name" 2>/dev/null || true
    fi
    echo "    -> $OUT/$name ($(du -h "$OUT/$name" | cut -f1))"
done

echo "all artifacts in $OUT/"
