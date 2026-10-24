@echo off
setlocal enabledelayedexpansion

REM ==============================================================================
REM build.bat - Build skopeo for multiple architectures
REM
REM Default targets: windows-amd64 (native), linux-amd64 + linux-arm64 (musl
REM static, cross-compiled with cargo-zigbuild).
REM
REM Requirements:
REM   - Rust toolchain (https://rustup.rs) with the MSVC Build Tools
REM   - For the Linux targets only: cargo-zigbuild + zig
REM       cargo install cargo-zigbuild
REM       pip install ziglang   (or: winget install zig.zig)
REM
REM Usage:
REM   build.bat                          REM default matrix into dist\
REM   build.bat x86_64-unknown-linux-musl aarch64-unknown-linux-musl
REM ==============================================================================

set "SCRIPT_DIR=%~dp0"
set "OUTDIR=%SCRIPT_DIR%dist"
set "DEFAULT_TARGETS=x86_64-pc-windows-msvc x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

set "TARGETS=%*"
if "%TARGETS%"=="" set "TARGETS=%DEFAULT_TARGETS%"

REM Embed the git commit into the binary (skopeo --version).
set "SKOPEO_COMMIT=unknown"
where git >nul 2>&1
if not errorlevel 1 (
    pushd "%SCRIPT_DIR%"
    for /f "usebackq delims=" %%i in (`git rev-parse --short HEAD 2^>nul`) do set "SKOPEO_COMMIT=%%i"
    popd
)

where cargo >nul 2>&1
if errorlevel 1 (
    echo [ERROR] cargo not found in PATH. Install Rust: https://rustup.rs
    exit /b 1
)

echo ==============================================================================
echo            skopeo multi-arch build
echo ==============================================================================

REM Make sure every requested target is installed for rustup (idempotent).
for %%T in (%TARGETS%) do (
    rustup target add %%T >nul 2>&1
)

if not exist "%OUTDIR%" mkdir "%OUTDIR%"

for %%T in (%TARGETS%) do (
    echo ==^> building %%T

    echo %%T | findstr /C:"linux-" >nul
    if not errorlevel 1 (
        REM Linux targets are cross-compiled with cargo-zigbuild (zig linker).
        where cargo-zigbuild >nul 2>&1
        if errorlevel 1 (
            echo [ERROR] cargo-zigbuild is required for Linux targets:
            echo           cargo install cargo-zigbuild
            echo           pip install ziglang    ^(or: winget install zig.zig^)
            exit /b 1
        )
        cargo zigbuild --release --target %%T
    ) else (
        cargo build --release --target %%T
    )
    if errorlevel 1 (
        echo [ERROR] build failed for %%T
        exit /b 1
    )

    set "EXT="
    echo %%T | findstr /C:"windows-" >nul
    if not errorlevel 1 set "EXT=.exe"

    copy /Y "%SCRIPT_DIR%target\%%T\release\skopeo!EXT!" "%OUTDIR%\skopeo-%%T!EXT!" >nul
    if errorlevel 1 (
        echo [ERROR] copy failed for %%T
        exit /b 1
    )
    for %%F in ("%OUTDIR%\skopeo-%%T!EXT!") do (
        set /a KB=%%~zF / 1024
        echo     %%F ^(!KB! KB^)
    )
)

echo.
echo     all artifacts in %OUTDIR%
exit /b 0
