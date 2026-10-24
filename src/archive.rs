//! Helpers for tar-based transports (`oci-archive:`, `docker-archive:`).

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Open a file, transparently decompressing gzip when the magic bytes match.
pub fn open_maybe_gzip(path: &Path) -> Result<Box<dyn Read>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buffered = BufReader::new(file);
    let is_gzip = {
        use std::io::BufRead;
        let buffer = buffered.fill_buf()?;
        buffer.len() >= 2 && buffer[0] == 0x1f && buffer[1] == 0x8b
    };
    if is_gzip {
        Ok(Box::new(flate2::read::GzDecoder::new(buffered)))
    } else {
        Ok(Box::new(buffered))
    }
}

/// Extract an archive into `dest`, rejecting entries that escape it.
pub fn extract_to_dir(archive: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let reader = open_maybe_gzip(archive)?;
    let mut tar = tar::Archive::new(reader);
    tar.set_overwrite(true);
    let entries = tar.entries().context("reading archive entries")?;
    for entry in entries {
        let mut entry = entry.context("reading archive entry")?;
        let path = entry.path().context("reading archive entry path")?.into_owned();
        let sanitized = sanitize(&path)?;
        if sanitized.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(&sanitized);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&target)
            .with_context(|| format!("creating {}", target.display()))?;
        io::copy(&mut entry, &mut out)
            .with_context(|| format!("extracting {}", path.display()))?;
    }
    Ok(())
}

fn sanitize(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => bail!("archive entry {} escapes the destination", path.display()),
            Component::RootDir | Component::Prefix(_) => {
                bail!("archive entry {} has an absolute path", path.display())
            }
        }
    }
    Ok(out)
}

/// Write `dir` as a tar archive at `target`, atomically.
pub fn write_dir_to_tar(dir: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary archive in {}", parent.display()))?;
    {
        let mut builder = tar::Builder::new(temp.as_file());
        builder.mode(tar::HeaderMode::Deterministic);
        add_dir(&mut builder, dir, Path::new(""))?;
        builder.finish().context("finishing tar archive")?;
    }
    temp.as_file().sync_all().ok();
    temp.persist(target)
        .map_err(|e| anyhow::anyhow!("writing {}: {}", target.display(), e.error))?;
    Ok(())
}

fn add_dir(builder: &mut tar::Builder<&File>, base: &Path, rel: &Path) -> Result<()> {
    let dir = base.join(rel);
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let path_rel = rel.join(&name);
        let tar_name = path_rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_mtime(0);
            header.set_cksum();
            let mut name_with_slash = tar_name.clone();
            name_with_slash.push('/');
            builder
                .append_data(&mut header, name_with_slash, io::empty())
                .with_context(|| format!("adding directory {tar_name} to archive"))?;
            add_dir(builder, base, &path_rel)?;
        } else if metadata.is_file() {
            let mut file = File::open(entry.path())?;
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(metadata.len());
            header.set_mtime(0);
            header.set_cksum();
            builder
                .append_data(&mut header, &tar_name, &mut file)
                .with_context(|| format!("adding {tar_name} to archive"))?;
        }
    }
    Ok(())
}

/// Add a single file to a tar builder under `name`.
pub fn append_bytes<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_size(data.len() as u64);
    header.set_mtime(0);
    header.set_cksum();
    builder.append_data(&mut header, name, data)?;
    Ok(())
}
