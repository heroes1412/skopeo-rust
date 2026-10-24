//! Small helpers shared by the CLI, transports and auth code.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// Parse durations in the Go style: `30s`, `10m`, `1h30m`, `100ms`.
pub fn parse_duration(value: &str) -> Result<Duration> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("empty duration");
    }
    let mut seconds = 0.0f64;
    let mut rest = value;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        if digits == 0 {
            anyhow::bail!("invalid duration {value:?}");
        }
        let n: f64 = rest[..digits]
            .parse()
            .with_context(|| format!("invalid duration {value:?}"))?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        let (unit, remainder) = rest.split_at(unit_len);
        rest = remainder;
        let factor = match unit {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            "" => 1.0,
            other => anyhow::bail!("invalid duration unit {other:?} in {value:?}"),
        };
        seconds += n * factor;
    }
    Ok(Duration::from_secs_f64(seconds))
}

/// `$XDG_CONFIG_HOME` or `~/.config` on Unix-like systems; `%APPDATA%` is not
/// used because containers tooling uses `$HOME/.config` on Windows too.
pub fn config_home() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    home_dir().map(|h| h.join(".config"))
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
}

pub fn runtime_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    None
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit + 1 < UNITS.len() {
        size /= 1000.0;
        unit += 1;
    }
    format!("{size:.3} {}", UNITS[unit])
}

/// Write a file through a temporary file in the same directory, then rename.
/// This keeps half-written manifests/layouts from being observed.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    tmp.write_all(data)?;
    tmp.flush()?;
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("renaming temporary file to {}: {}", path.display(), e.error))?;
    Ok(())
}

pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    atomic_write(path, &data)
}

pub fn json_pretty<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

/// Set Unix mode 0600 where applicable (no-op elsewhere).
pub fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Set Unix mode 0644 where applicable (no-op elsewhere).
pub fn set_public_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o644);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Environment variable lookup, treating empty values as unset.
pub fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Temporary work directory, honouring `--tmpdir`.
pub fn temp_dir(parent: Option<&Path>, prefix: &str) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    match parent {
        Some(dir) => {
            fs::create_dir_all(dir)
                .with_context(|| format!("creating temporary directory {}", dir.display()))?;
            Ok(builder.tempdir_in(dir)?)
        }
        None => Ok(builder.tempdir()?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("100ms").unwrap(), Duration::from_millis(100));
        assert_eq!(parse_duration("5").unwrap(), Duration::from_secs(5));
        assert!(parse_duration("nope").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.000 kB");
        assert_eq!(human_size(1_500_000), "1.500 MB");
    }
}

/// Render JSON the way Go's `json.MarshalIndent(v, "", "    ")` + `\n` does
/// (4-space indent, trailing newline).
pub fn go_json_string(value: &serde_json::Value) -> Result<String> {
    let pretty = serde_json::to_string_pretty(value)?;
    let mut out = String::with_capacity(pretty.len() * 2);
    for line in pretty.lines() {
        let indent = (line.len() - line.trim_start().len()) * 2;
        for _ in 0..indent {
            out.push(' ');
        }
        out.push_str(line.trim_start());
        out.push('\n');
    }
    Ok(out)
}

fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// slog-style log line as skopeo prints it on stderr: `LEVEL message k=v ...`,
/// prefixed by `YYYY/MM/DD HH:MM:SS ` when stderr is not a terminal (Go's
/// `log` flags; the Go timestamp is local time, this port uses UTC).
pub fn slog_line(level: &str, message: &str, attrs: &[(&str, String)]) {
    let mut line = String::new();
    if !stderr_is_tty() {
        line.push_str(&log_timestamp());
        line.push(' ');
    }
    line.push_str(level);
    line.push(' ');
    line.push_str(message);
    for (key, value) in attrs {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(value);
    }
    eprintln!("{line}");
}

/// `YYYY/MM/DD HH:MM:SS` for the current time (UTC).
pub fn log_timestamp() -> String {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}/{:02}/{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// RFC3339 (UTC) for a unix timestamp.
pub fn rfc3339_from_unix(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Howard Hinnant's civil-from-days algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod go_json_tests {
    use super::*;

    #[test]
    fn four_space_indent() {
        let value = serde_json::json!({"A": 1, "B": [true, null]});
        let out = go_json_string(&value).unwrap();
        assert!(out.starts_with("{\n    \"A\": 1,\n    \"B\": [\n        true,"));
        assert!(out.ends_with('\n'));
    }
}
