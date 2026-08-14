use anyhow::{Context, Result, bail};
use backhand::{FilesystemReader, InnerNode};
use dwarfs::archive::{Config, IsInode};
use dwarfs::{Archive, ArchiveIndex, AsChunks, Inode, InodeKind};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

const APPIMAGE_MAGIC_OFFSET: u64 = 8;
const SHT_NOBITS: u32 = 8;

pub fn image_type(path: &Path) -> Result<Option<u8>> {
    let mut file = File::open(path)?;
    let mut magic = [0_u8; 11];
    if file.read(&mut magic)? < magic.len() {
        return Ok(None);
    }
    if &magic[..4] != b"\x7fELF" || &magic[APPIMAGE_MAGIC_OFFSET as usize..10] != b"AI" {
        return Ok(None);
    }
    Ok(Some(magic[10]))
}

pub fn extract(path: &Path, dest: &Path) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("AppImage is supported only on Linux")
    }
    let offset = payload_offset(path)?;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut magic = [0_u8; 8];
    file.read_exact(&mut magic)?;
    match &magic[..4] {
        b"hsqs" => extract_squashfs(path, offset, dest)?,
        _ if &magic[..6] == b"DWARFS" && magic[6] == 2 && (3..=5).contains(&magic[7]) => {
            extract_dwarfs(path, offset, dest)?
        }
        _ => bail!("unsupported AppImage filesystem at offset {offset}"),
    }
    validate_apprun(dest)
}

fn payload_offset(path: &Path) -> Result<u64> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut header = [0_u8; 64];
    file.read_exact(&mut header)
        .context("truncated AppImage ELF header")?;
    if &header[..4] != b"\x7fELF" {
        bail!("AppImage is not an ELF file")
    }
    match &header[APPIMAGE_MAGIC_OFFSET as usize..APPIMAGE_MAGIC_OFFSET as usize + 3] {
        b"AI\x01" => bail!("type-1 AppImage is not supported"),
        b"AI\x02" => {}
        _ => bail!("invalid AppImage type-2 magic"),
    }
    if header[5] != 1 {
        bail!("big-endian AppImage ELF is not supported")
    }
    if header[6] != 1 || le_u32(&header, 20)? != 1 {
        bail!("invalid AppImage ELF version")
    }
    if !matches!(le_u16(&header, 16)?, 2 | 3) {
        bail!("AppImage ELF is not an executable or shared object")
    }

    let (class, ehsize, phoff, phentsize, phnum, shoff, shentsize, shnum) = match header[4] {
        1 => (
            1_u8,
            u64::from(le_u16(&header, 40)?),
            u64::from(le_u32(&header, 28)?),
            u64::from(le_u16(&header, 42)?),
            u64::from(le_u16(&header, 44)?),
            u64::from(le_u32(&header, 32)?),
            u64::from(le_u16(&header, 46)?),
            u64::from(le_u16(&header, 48)?),
        ),
        2 => (
            2_u8,
            u64::from(le_u16(&header, 52)?),
            le_u64(&header, 32)?,
            u64::from(le_u16(&header, 54)?),
            u64::from(le_u16(&header, 56)?),
            le_u64(&header, 40)?,
            u64::from(le_u16(&header, 58)?),
            u64::from(le_u16(&header, 60)?),
        ),
        other => bail!("unsupported AppImage ELF class {other}"),
    };
    let min_ph = if class == 1 { 32 } else { 56 };
    let min_sh = if class == 1 { 40 } else { 64 };
    let min_eh = if class == 1 { 52 } else { 64 };
    if ehsize < min_eh || ehsize > length {
        bail!("invalid AppImage ELF header size")
    }
    if phnum == 0xffff || (shoff != 0 && shnum == 0) {
        bail!("extended AppImage ELF table counts are not supported")
    }
    if (phnum != 0 && phentsize < min_ph) || (shnum != 0 && shentsize < min_sh) {
        bail!("invalid AppImage ELF table entry size")
    }

    let ph_end = table_end(phoff, phentsize, phnum, length)?;
    let sh_end = table_end(shoff, shentsize, shnum, length)?;
    let mut end = ehsize.max(ph_end).max(sh_end);

    for index in 0..phnum {
        let entry = read_entry(&mut file, phoff, phentsize, index, length)?;
        let (offset, size) = if class == 1 {
            (
                u64::from(le_u32(&entry, 4)?),
                u64::from(le_u32(&entry, 16)?),
            )
        } else {
            (le_u64(&entry, 8)?, le_u64(&entry, 32)?)
        };
        end = end.max(checked_end(offset, size, length)?);
    }
    for index in 0..shnum {
        let entry = read_entry(&mut file, shoff, shentsize, index, length)?;
        let section_type = le_u32(&entry, 4)?;
        if section_type == SHT_NOBITS {
            continue;
        }
        let (offset, size) = if class == 1 {
            (
                u64::from(le_u32(&entry, 16)?),
                u64::from(le_u32(&entry, 20)?),
            )
        } else {
            (le_u64(&entry, 24)?, le_u64(&entry, 32)?)
        };
        end = end.max(checked_end(offset, size, length)?);
    }
    if end >= length {
        bail!("AppImage has no embedded filesystem")
    }
    Ok(end)
}

fn table_end(offset: u64, entry_size: u64, count: u64, length: u64) -> Result<u64> {
    if count == 0 {
        return Ok(0);
    }
    if offset == 0 {
        bail!("invalid AppImage ELF table offset")
    }
    checked_end(
        offset,
        entry_size
            .checked_mul(count)
            .context("AppImage ELF table size overflow")?,
        length,
    )
}

fn checked_end(offset: u64, size: u64, length: u64) -> Result<u64> {
    let end = offset
        .checked_add(size)
        .context("AppImage ELF range overflow")?;
    if end > length {
        bail!("AppImage ELF range exceeds file length")
    }
    Ok(end)
}

fn read_entry(file: &mut File, offset: u64, size: u64, index: u64, length: u64) -> Result<Vec<u8>> {
    let start = offset
        .checked_add(
            size.checked_mul(index)
                .context("AppImage ELF table overflow")?,
        )
        .context("AppImage ELF table overflow")?;
    checked_end(start, size, length)?;
    let size = usize::try_from(size).context("AppImage ELF entry is too large")?;
    let mut bytes = vec![0; size];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .context("truncated AppImage ELF field")?
            .try_into()?,
    ))
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .context("truncated AppImage ELF field")?
            .try_into()?,
    ))
}

fn le_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .context("truncated AppImage ELF field")?
            .try_into()?,
    ))
}

fn extract_squashfs(path: &Path, offset: u64, dest: &Path) -> Result<()> {
    let filesystem =
        FilesystemReader::from_reader_with_offset(BufReader::new(File::open(path)?), offset)
            .context("read AppImage SquashFS")?;
    let mut entries = HashMap::new();
    let mut symlinks = HashSet::new();
    for node in filesystem.files() {
        let Some(relative) = squashfs_path(&node.fullpath)? else {
            continue;
        };
        let kind = match &node.inner {
            InnerNode::Dir(_) => EntryKind::Directory,
            InnerNode::File(_) => EntryKind::File,
            InnerNode::Symlink(link) => {
                validate_target(relative.parent().unwrap_or(Path::new("")), &link.link)?;
                symlinks.insert(relative.clone());
                EntryKind::Symlink
            }
            _ => bail!("unsupported special SquashFS entry {}", relative.display()),
        };
        if entries.insert(relative.clone(), kind).is_some() {
            bail!("duplicate SquashFS entry {}", relative.display())
        }
    }
    reject_symlink_ancestors(entries.keys(), &symlinks)?;

    for node in filesystem.files() {
        let Some(relative) = squashfs_path(&node.fullpath)? else {
            continue;
        };
        if matches!(node.inner, InnerNode::Dir(_)) {
            fs::create_dir_all(dest.join(relative))?;
        }
    }
    for node in filesystem.files() {
        let Some(relative) = squashfs_path(&node.fullpath)? else {
            continue;
        };
        if let InnerNode::File(file) = &node.inner {
            let out = dest.join(relative);
            create_parent(&out)?;
            io::copy(
                &mut filesystem.file(file).reader(),
                &mut File::create(&out)?,
            )?;
            set_mode(&out, u32::from(node.header.permissions))?;
        }
    }
    for node in filesystem.files() {
        let Some(relative) = squashfs_path(&node.fullpath)? else {
            continue;
        };
        if let InnerNode::Symlink(link) = &node.inner {
            let out = dest.join(relative);
            create_parent(&out)?;
            symlink(&link.link, out)?;
        }
    }
    apply_squashfs_directory_modes(&filesystem, dest)
}

fn apply_squashfs_directory_modes(filesystem: &FilesystemReader<'_>, dest: &Path) -> Result<()> {
    let mut directories = Vec::new();
    for node in filesystem.files() {
        if matches!(node.inner, InnerNode::Dir(_))
            && let Some(path) = squashfs_path(&node.fullpath)?
        {
            directories.push((path, node));
        }
    }
    directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, node) in directories {
        set_mode(&dest.join(path), u32::from(node.header.permissions))?;
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EntryKind {
    Directory,
    File,
    Symlink,
}

struct DwarfEntry<'a> {
    path: PathBuf,
    inode: Inode<'a>,
}

fn extract_dwarfs(path: &Path, offset: u64, dest: &Path) -> Result<()> {
    let mut config = Config::default();
    config
        .metadata_size_limit(64 << 20)
        .block_cache_size_limit(128 << 20);
    let mut sections = dwarfs::section::SectionReader::new_with_offset(File::open(path)?, offset);
    let index = ArchiveIndex::new_with_config(&mut sections, &config)
        .context("read AppImage DwarFS index")?;
    let mut archive = Archive::new_with_index_and_config(sections, &index, &config)
        .context("read AppImage DwarFS")?;
    let mut entries = Vec::new();
    let mut visited_directories = HashSet::new();
    collect_dwarfs(
        index.root().into(),
        Path::new(""),
        &mut entries,
        &mut visited_directories,
    )?;
    let mut paths = HashSet::new();
    for entry in &entries {
        if !paths.insert(entry.path.clone()) {
            bail!("duplicate DwarFS entry {}", entry.path.display())
        }
    }

    let symlinks = entries
        .iter()
        .filter(|entry| matches!(entry.inode.classify(), InodeKind::Symlink(_)))
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    reject_symlink_ancestors(entries.iter().map(|entry| &entry.path), &symlinks)?;

    for entry in &entries {
        if matches!(entry.inode.classify(), InodeKind::Directory(_)) {
            fs::create_dir_all(dest.join(&entry.path))?;
        }
    }
    let mut files = entries
        .iter()
        .filter_map(|entry| match entry.inode.classify() {
            InodeKind::File(file) => Some((entry, file)),
            _ => None,
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|(_, file)| {
        file.as_chunks()
            .next()
            .map_or(u32::MAX, |chunk| chunk.section_idx())
    });
    let mut materialized_files = HashMap::<u32, PathBuf>::new();
    for (entry, file) in files {
        let out = dest.join(&entry.path);
        create_parent(&out)?;
        let inode_num = entry.inode.inode_num();
        if let Some(existing) = materialized_files.get(&inode_num) {
            fs::hard_link(dest.join(existing), &out)?;
        } else {
            io::copy(&mut file.as_reader(&mut archive), &mut File::create(&out)?)?;
            set_mode(
                &out,
                entry.inode.metadata().file_type_mode().permission_bits(),
            )?;
            materialized_files.insert(inode_num, entry.path.clone());
        }
    }
    for entry in &entries {
        if let InodeKind::Symlink(link) = entry.inode.classify() {
            let out = dest.join(&entry.path);
            create_parent(&out)?;
            symlink(link.target(), out)?;
        }
    }
    let mut directories = entries
        .iter()
        .filter(|entry| matches!(entry.inode.classify(), InodeKind::Directory(_)))
        .collect::<Vec<_>>();
    directories.sort_by_key(|entry| std::cmp::Reverse(entry.path.components().count()));
    for entry in directories {
        set_mode(
            &dest.join(&entry.path),
            entry.inode.metadata().file_type_mode().permission_bits(),
        )?;
    }
    Ok(())
}

fn collect_dwarfs<'a>(
    inode: Inode<'a>,
    parent: &Path,
    entries: &mut Vec<DwarfEntry<'a>>,
    visited_directories: &mut HashSet<u32>,
) -> Result<()> {
    let Some(directory) = inode.as_dir() else {
        bail!("DwarFS root is not a directory")
    };
    if !visited_directories.insert(inode.inode_num()) {
        bail!("DwarFS directory is referenced more than once")
    }
    for entry in directory.entries() {
        let component = safe_component(entry.name())?;
        let path = parent.join(component);
        let child = entry.inode();
        match child.classify() {
            InodeKind::Device(_) | InodeKind::Ipc(_) => {
                bail!("unsupported special DwarFS entry {}", path.display())
            }
            InodeKind::Symlink(link) => {
                validate_target(
                    path.parent().unwrap_or(Path::new("")),
                    Path::new(link.target()),
                )?;
            }
            _ => {}
        }
        entries.push(DwarfEntry {
            path: path.clone(),
            inode: child,
        });
        if child.is_dir() {
            collect_dwarfs(child, &path, entries, visited_directories)?;
        }
    }
    Ok(())
}

fn squashfs_path(path: &Path) -> Result<Option<PathBuf>> {
    if path == Path::new("/") {
        return Ok(None);
    }
    let relative = path
        .strip_prefix("/")
        .context("SquashFS path is not rooted")?;
    safe_relative(relative).map(Some)
}

fn safe_component(name: &str) -> Result<&str> {
    if name.is_empty() || name.contains('/') || name.contains('\0') || matches!(name, "." | "..") {
        bail!("unsafe DwarFS entry name {name:?}")
    }
    Ok(name)
}

fn safe_relative(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        bail!("absolute AppImage path {}", path.display())
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => clean.push(value),
            _ => bail!("AppImage path escapes extraction root: {}", path.display()),
        }
    }
    if clean.as_os_str().is_empty() {
        bail!("empty AppImage path")
    }
    Ok(clean)
}

fn validate_target(parent: &Path, target: &Path) -> Result<()> {
    if target.is_absolute() {
        bail!("absolute AppImage symlink target {}", target.display())
    }
    let mut resolved = parent.to_path_buf();
    for component in target.components() {
        match component {
            Component::Normal(value) => resolved.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    bail!("AppImage symlink escapes extraction root")
                }
            }
            _ => bail!("unsafe AppImage symlink target"),
        }
    }
    Ok(())
}

fn reject_symlink_ancestors<'a>(
    paths: impl IntoIterator<Item = &'a PathBuf>,
    symlinks: &HashSet<PathBuf>,
) -> Result<()> {
    for path in paths {
        if path
            .ancestors()
            .skip(1)
            .any(|parent| symlinks.contains(parent))
        {
            bail!("AppImage entry traverses symlink: {}", path.display())
        }
    }
    Ok(())
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))?;
    Ok(())
}

fn validate_apprun(dest: &Path) -> Result<()> {
    let path = dest.join("AppRun");
    let metadata = fs::metadata(&path).context("AppImage has no usable root AppRun")?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("AppImage root AppRun is not executable")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_paths_and_links() {
        assert!(safe_relative(Path::new("usr/bin/tool")).is_ok());
        assert!(safe_relative(Path::new("../tool")).is_err());
        assert!(validate_target(Path::new("usr/bin"), Path::new("../lib/tool")).is_ok());
        assert!(validate_target(Path::new(""), Path::new("../tool")).is_err());
    }

    #[test]
    fn elf_layout_determines_payload_offset_instead_of_magic_scanning() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut image = vec![0_u8; 68];
        image[..4].copy_from_slice(b"\x7fELF");
        image[4] = 2;
        image[5] = 1;
        image[6] = 1;
        image[8..11].copy_from_slice(b"AI\x02");
        image[16..18].copy_from_slice(&2_u16.to_le_bytes());
        image[20..24].copy_from_slice(&1_u32.to_le_bytes());
        image[52..54].copy_from_slice(&64_u16.to_le_bytes());
        image[64..68].copy_from_slice(b"hsqs");
        fs::write(temp.path(), image).unwrap();
        assert_eq!(payload_offset(temp.path()).unwrap(), 64);
    }

    #[test]
    fn type_one_magic_is_identified() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut image = vec![0_u8; 64];
        image[..4].copy_from_slice(b"\x7fELF");
        image[8..11].copy_from_slice(b"AI\x01");
        fs::write(temp.path(), image).unwrap();
        assert_eq!(image_type(temp.path()).unwrap(), Some(1));
    }

    #[test]
    fn extracts_external_validation_image_when_requested() {
        let Some(path) = std::env::var_os("EGET_TEST_APPIMAGE") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        extract(Path::new(&path), temp.path()).unwrap();
        assert!(temp.path().join("AppRun").exists());
    }
}
