@echo off
setlocal enabledelayedexpansion

REM ==============================================================================
REM clean.bat - Remove build artifacts from the skopeo Rust tree
REM
REM Removes:
REM   target\    cargo build directory (~2 GB)
REM   dist\      built binaries from build.bat / build.ps1 / build.sh
REM   layers-*   blobs dropped in the tree by "skopeo layers"
REM
REM Keeps: src\, tests\, Cargo.toml, Cargo.lock (needed for reproducible builds).
REM ==============================================================================

set "SCRIPT_DIR=%~dp0"
set "REMOVED=0"

echo ==============================================================================
echo            skopeo build cleanup
echo ==============================================================================

call :remove_dir target
call :remove_dir dist

for /d %%D in ("%SCRIPT_DIR%layers-*") do (
    rmdir /s /q "%%D" 2>nul
    if not exist "%%D" (
        echo [OK] removed %%D
    ) else (
        echo [WARN] could not fully remove %%D
    )
)
for %%F in ("%SCRIPT_DIR%layers-*") do (
    if exist "%%F" del /q "%%F" 2>nul
)

if %REMOVED%==0 (
    echo     nothing fully removed - tree was already clean or folders are locked
) else (
    echo.
    echo     cleaned %REMOVED% folder^(s^)
)
exit /b 0

:remove_dir
set "DIR=%SCRIPT_DIR%%~1"
if not exist "%DIR%" exit /b 0

REM Delete the contents first so a lingering handle on the folder itself does
REM not block the cleanup.
del /q /f "%DIR%\*" >nul 2>&1
for /d %%D in ("%DIR%\*") do rmdir /s /q "%%D" >nul 2>&1
rmdir /s /q "%DIR%" >nul 2>&1

if exist "%DIR%" (
    dir /b "%DIR%" 2>nul | findstr /r "." >nul
    if errorlevel 1 (
        echo [WARN] folder "%~1" is empty but locked by another process; delete it manually later
    ) else (
        echo [WARN] could not fully remove "%~1" - a build process may still be running
    )
) else (
    echo [OK] removed folder "%~1"
    set /a REMOVED+=1
)
exit /b 0
