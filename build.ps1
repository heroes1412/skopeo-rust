# ==============================================================================
# Script: build.ps1
# Description: Build skopeo for multiple architectures (Windows / Linux / macOS)
#
# Default targets: windows-amd64, linux-amd64 (musl static), linux-arm64 (musl static)
# Requires: Rust toolchain; for the Linux targets: cargo-zigbuild + zig
#   cargo install cargo-zigbuild
#   pip install ziglang      (or: winget install zig.zig)
#
# Usage:
#   .\build.ps1                              # default matrix into .\dist\
#   .\build.ps1 -Targets x86_64-unknown-linux-musl
#   .\build.ps1 -OutDir C:\temp\bins
# ==============================================================================
[CmdletBinding()]
param(
    [string]$OutDir = "",
    [string[]]$Targets = @(
        "x86_64-pc-windows-msvc",          # windows amd64 (native)
        "x86_64-unknown-linux-musl",       # linux amd64 (static, via cargo-zigbuild)
        "aarch64-unknown-linux-musl"       # linux arm64 (static, via cargo-zigbuild)
    )
)

$ErrorActionPreference = "Stop"

# Script directory (PS 5.1-safe)
$ScriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
if (-not $OutDir) { $OutDir = Join-Path $ScriptDir "dist" }

function Write-Step([string]$msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok([string]$msg)   { Write-Host "    $msg" -ForegroundColor Green }

# 1. Toolchain checks
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not found in PATH. Install Rust: https://rustup.rs"
}

$linuxTargets = $Targets | Where-Object { $_ -match 'linux-(musl|gnu)' }
if ($linuxTargets) {
    $zigbuildOk = $false
    try { & cargo zigbuild --help *> $null; $zigbuildOk = ($LASTEXITCODE -eq 0) } catch {}
    if (-not $zigbuildOk) {
        Write-Error "cargo-zigbuild is required for Linux targets. Install with:`n  cargo install cargo-zigbuild`n  pip install ziglang  (or: winget install zig.zig)"
    }
}

# 2. Ensure rustup targets exist
foreach ($t in $Targets) {
    $installed = & rustup target list --installed
    if ($installed -notcontains $t) {
        Write-Step "adding rustup target $t"
        & rustup target add $t
        if ($LASTEXITCODE -ne 0) { Write-Error "rustup target add $t failed" }
    }
}

# Embed the git commit into the binary (skopeo --version).
if (-not $env:SKOPEO_COMMIT) {
    $git = Get-Command git -ErrorAction SilentlyContinue
    if ($git) {
        $sha = & git -C $ScriptDir rev-parse --short HEAD 2>$null
        $env:SKOPEO_COMMIT = if ($LASTEXITCODE -eq 0) { $sha } else { "unknown" }
    } else {
        $env:SKOPEO_COMMIT = "unknown"
    }
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

# 3. Build each target
foreach ($t in $Targets) {
    Write-Step "building $t"
    if ($t -match 'linux-(musl|gnu)') {
        & cargo zigbuild --release --target $t
    } else {
        & cargo build --release --target $t
    }
    if ($LASTEXITCODE -ne 0) { Write-Error "build failed for $t" }

    $ext = if ($t -like '*windows*') { ".exe" } else { "" }
    $src = Join-Path "$PSScriptRoot\target\$t\release" "skopeo$ext"
    $dst = Join-Path $OutDir "skopeo-$($t -replace '-unknown-','-')$ext"
    Copy-Item $src $dst -Force

    # Strip symbols on non-Windows targets when strip is available
    if ($ext -eq "" -and (Get-Command strip -ErrorAction SilentlyContinue)) {
        & strip $dst 2>$null
    }
    $size = "{0:N2} MB" -f ((Get-Item $dst).Length / 1MB)
    Write-Ok "$dst ($size)"
}

Write-Host ""
Write-Ok "all artifacts in $OutDir"
