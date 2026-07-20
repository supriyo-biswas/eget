use anyhow::{Context, Result, bail};
use bzip2::read::BzDecoder;
use flate2::read::MultiGzDecoder;
use sevenz_rust2::{ArchiveReader, Password};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use tar::EntryType;
use walkdir::WalkDir;
use xz2::read::XzDecoder;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    SevenZ,
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
    Gz,
    Bz2,
    Xz,
    Zst,
    Plain,
}

pub fn format(name: &str) -> Format {
    let n = name.to_ascii_lowercase();
    for (suffix, kind) in [
        (".7z", Format::SevenZ),
        (".tar.gz", Format::TarGz),
        (".tgz", Format::TarGz),
        (".tar.bz2", Format::TarBz2),
        (".tbz2", Format::TarBz2),
        (".tbz", Format::TarBz2),
        (".tar.xz", Format::TarXz),
        (".txz", Format::TarXz),
        (".tar.zst", Format::TarZst),
        (".tzst", Format::TarZst),
        (".zip", Format::Zip),
        (".tar", Format::Tar),
        (".gz", Format::Gz),
        (".bz2", Format::Bz2),
        (".xz", Format::Xz),
        (".zst", Format::Zst),
    ] {
        if n.ends_with(suffix) {
            return kind;
        }
    }
    Format::Plain
}

pub fn extract(payload: &Path, source_name: &str, app: &str, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o700))?;
    match format(source_name) {
        Format::SevenZ => extract_7z(payload, dest)?,
        Format::Zip => extract_zip(payload, dest)?,
        f @ (Format::Tar | Format::TarGz | Format::TarBz2 | Format::TarXz | Format::TarZst) => {
            validate_tar(payload, f)?;
            unpack_tar(payload, f, dest)?;
        }
        Format::Gz => extract_one(MultiGzDecoder::new(File::open(payload)?), app, dest)?,
        Format::Bz2 => extract_one(BzDecoder::new(File::open(payload)?), app, dest)?,
        Format::Xz => extract_one(XzDecoder::new(File::open(payload)?), app, dest)?,
        Format::Zst => extract_one(
            zstd::stream::read::Decoder::new(File::open(payload)?)?,
            app,
            dest,
        )?,
        Format::Plain => {
            fs::copy(payload, dest.join(app))?;
            fs::set_permissions(dest.join(app), fs::Permissions::from_mode(0o755))?;
        }
    }
    containment_walk(dest)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SevenZKind {
    Directory,
    File,
    Symlink,
}

struct SevenZEntry {
    name: String,
    path: PathBuf,
    kind: SevenZKind,
    mode: u32,
}

fn extract_7z(path: &Path, dest: &Path) -> Result<()> {
    let mut archive = ArchiveReader::open(path, Password::empty())?;

    let mut entries = Vec::with_capacity(archive.archive().files.len());
    let mut symlinks = HashSet::new();
    for entry in &archive.archive().files {
        if entry.is_anti_item() {
            bail!("unsupported 7z anti-item {}", entry.name())
        }
        let rel = safe_7z_name(entry.name())?;
        let unix_mode = entry.windows_attributes() >> 16;
        let file_type = unix_mode & 0o170000;
        let kind = if entry.is_directory() {
            if !matches!(file_type, 0 | 0o040000) {
                bail!("conflicting 7z entry type {}", rel.display())
            }
            SevenZKind::Directory
        } else {
            match file_type {
                0 | 0o100000 => SevenZKind::File,
                0o120000 => SevenZKind::Symlink,
                _ => bail!("unsupported special 7z entry {}", rel.display()),
            }
        };
        if kind == SevenZKind::Symlink {
            symlinks.insert(rel.clone());
        }
        let default_mode = if kind == SevenZKind::Directory {
            0o755
        } else {
            0o644
        };
        entries.push(SevenZEntry {
            name: entry.name().to_owned(),
            path: rel,
            kind,
            mode: if unix_mode == 0 {
                default_mode
            } else {
                unix_mode & 0o777
            },
        });
    }

    let mut seen = HashMap::new();
    for entry in &entries {
        if let Some(old) = seen.insert(entry.path.clone(), entry.kind)
            && old != entry.kind
        {
            bail!("conflicting duplicate 7z entry {}", entry.path.display())
        }
        if entry
            .path
            .ancestors()
            .skip(1)
            .any(|ancestor| symlinks.contains(ancestor))
        {
            bail!("7z writes through symlink {}", entry.path.display())
        }
    }

    let entry_indexes = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.name.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut extracted = 0;
    archive.for_each_entries(|archive_entry, reader| {
        let index = entry_indexes.get(archive_entry.name()).ok_or_else(|| {
            sevenz_rust2::Error::from(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected 7z entry",
            ))
        })?;
        let entry = &entries[*index];
        extracted += 1;
        let out = dest.join(&entry.path);
        match entry.kind {
            SevenZKind::Directory => fs::create_dir_all(&out)?,
            SevenZKind::File => {
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                io::copy(reader, &mut File::create(&out)?)?;
                fs::set_permissions(&out, fs::Permissions::from_mode(entry.mode))?;
            }
            SevenZKind::Symlink => {
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes)?;
                let target = PathBuf::from(
                    String::from_utf8(bytes)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                );
                validate_target(entry.path.parent().unwrap_or(Path::new("")), &target)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                symlink(target, out)?;
            }
        }
        Ok(true)
    })?;
    if extracted != entries.len() {
        bail!("7z archive entry count changed during extraction")
    }

    let mut directories = entries
        .iter()
        .filter(|entry| entry.kind == SevenZKind::Directory)
        .collect::<Vec<_>>();
    directories.sort_by_key(|entry| std::cmp::Reverse(entry.path.components().count()));
    for entry in directories {
        fs::set_permissions(
            dest.join(&entry.path),
            fs::Permissions::from_mode(entry.mode),
        )?;
    }
    Ok(())
}

fn safe_7z_name(name: &str) -> Result<PathBuf> {
    safe_relative(Path::new(&name.replace('\\', "/")))
}

fn extract_one(mut reader: impl Read, app: &str, dest: &Path) -> Result<()> {
    let out = dest.join(app);
    io::copy(&mut reader, &mut File::create(&out)?)?;
    fs::set_permissions(out, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
fn tar_reader(path: &Path, f: Format) -> Result<Box<dyn Read>> {
    let file = File::open(path)?;
    Ok(match f {
        Format::Tar => Box::new(file),
        Format::TarGz => Box::new(MultiGzDecoder::new(file)),
        Format::TarBz2 => Box::new(BzDecoder::new(file)),
        Format::TarXz => Box::new(XzDecoder::new(file)),
        Format::TarZst => Box::new(zstd::stream::read::Decoder::new(file)?),
        _ => unreachable!(),
    })
}

fn safe_relative(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        bail!("archive contains absolute path {}", path.display());
    }
    let mut clean = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(x) => clean.push(x),
            Component::CurDir => {}
            _ => bail!("archive path escapes extraction root: {}", path.display()),
        }
    }
    if clean.as_os_str().is_empty() {
        bail!("archive contains empty path");
    }
    Ok(clean)
}
fn validate_target(parent: &Path, target: &Path) -> Result<PathBuf> {
    if target.is_absolute() {
        bail!("absolute archive link target {}", target.display())
    }
    let mut out = parent.to_path_buf();
    for c in target.components() {
        match c {
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    bail!("archive link escapes extraction root")
                }
            }
            _ => bail!("unsafe archive link"),
        }
    }
    Ok(out)
}

fn validate_tar(path: &Path, f: Format) -> Result<()> {
    let mut archive = tar::Archive::new(tar_reader(path, f)?);
    let mut types = HashMap::<PathBuf, EntryType>::new();
    let mut symlinks = HashSet::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let p = safe_relative(&entry.path()?)?;
        let ty = entry.header().entry_type();
        if !(ty.is_file() || ty.is_dir() || ty.is_symlink() || ty.is_hard_link()) {
            bail!("unsupported special archive entry {}", p.display())
        }
        if let Some(old) = types.insert(p.clone(), ty)
            && old != ty
        {
            bail!("conflicting duplicate archive entry {}", p.display())
        }
        if p.ancestors().skip(1).any(|x| symlinks.contains(x)) {
            bail!("archive writes through symlink {}", p.display())
        }
        if ty.is_symlink() || ty.is_hard_link() {
            let target = entry.link_name()?.context("archive link has no target")?;
            validate_target(p.parent().unwrap_or(Path::new("")), &target)?;
            if ty.is_symlink() {
                symlinks.insert(p);
            }
        }
    }
    Ok(())
}
fn unpack_tar(path: &Path, f: Format, dest: &Path) -> Result<()> {
    let mut a = tar::Archive::new(tar_reader(path, f)?);
    a.set_preserve_permissions(true);
    a.set_preserve_ownerships(false);
    a.set_unpack_xattrs(false);
    a.unpack(dest)?;
    strip_special_bits(dest)
}

fn extract_zip(path: &Path, dest: &Path) -> Result<()> {
    let mut z = zip::ZipArchive::new(File::open(path)?)?;
    let mut seen = HashMap::new();
    let mut symlinks = HashSet::new();
    for i in 0..z.len() {
        let mut e = z.by_index(i)?;
        if e.encrypted() {
            bail!("encrypted ZIP entry is not supported: {}", e.name())
        }
        let rel = safe_relative(&e.enclosed_name().context("unsafe ZIP path")?)?;
        let mode = e
            .unix_mode()
            .unwrap_or(if e.is_dir() { 0o755 } else { 0o644 });
        let file_type = mode & 0o170000;
        if !matches!(file_type, 0 | 0o040000 | 0o100000 | 0o120000) {
            bail!("unsupported special ZIP entry {}", rel.display())
        }
        let is_link = file_type == 0o120000;
        let kind = if e.is_dir() {
            1
        } else if is_link {
            2
        } else {
            3
        };
        if let Some(old) = seen.insert(rel.clone(), kind)
            && old != kind
        {
            bail!("conflicting duplicate ZIP entry {}", rel.display())
        }
        if rel.ancestors().skip(1).any(|x| symlinks.contains(x)) {
            bail!("ZIP writes through symlink {}", rel.display())
        }
        let out = dest.join(&rel);
        if e.is_dir() {
            fs::create_dir_all(&out)?;
        } else if is_link {
            let mut bytes = Vec::new();
            e.read_to_end(&mut bytes)?;
            let target = PathBuf::from(String::from_utf8(bytes)?);
            validate_target(rel.parent().unwrap_or(Path::new("")), &target)?;
            if let Some(p) = out.parent() {
                fs::create_dir_all(p)?
            }
            symlink(target, &out)?;
            symlinks.insert(rel);
        } else {
            if let Some(p) = out.parent() {
                fs::create_dir_all(p)?
            }
            io::copy(&mut e, &mut File::create(&out)?)?;
            fs::set_permissions(&out, fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    strip_special_bits(dest)
}
fn strip_special_bits(root: &Path) -> Result<()> {
    for e in WalkDir::new(root).follow_links(false) {
        let e = e?;
        if e.file_type().is_symlink() {
            continue;
        }
        let mode = e.metadata()?.permissions().mode() & 0o777;
        fs::set_permissions(e.path(), fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}
fn containment_walk(root: &Path) -> Result<()> {
    let canonical = root.canonicalize()?;
    for e in WalkDir::new(root).follow_links(false) {
        let e = e?;
        if e.file_type().is_symlink() {
            let target = fs::canonicalize(e.path())
                .with_context(|| format!("resolve archive symlink {}", e.path().display()))?;
            if !target.starts_with(&canonical) {
                bail!(
                    "archive symlink escapes extraction root: {}",
                    e.path().display()
                )
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_dotdot() {
        assert!(safe_relative(Path::new("../oops")).is_err())
    }
    #[test]
    fn rejects_windows_style_7z_dotdot() {
        assert!(safe_7z_name("..\\oops").is_err())
    }
    #[test]
    fn suffixes() {
        assert_eq!(format("x.7Z"), Format::SevenZ);
        assert_eq!(format("x.tzst"), Format::TarZst);
        assert_eq!(format("tool-linux"), Format::Plain)
    }
}
