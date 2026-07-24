use anyhow::{Context, Result};
use object::read::elf::{ElfFile, ElfFile64, FileHeader, ProgramHeader};
use object::read::macho::{FatArch, MachOFatFile32, MachOFatFile64};
use object::read::{ReadCache, ReadRef};
use object::{Architecture, BinaryFormat, FileKind, Object, ObjectKind};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostOs {
    Linux,
    Macos,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostArch {
    X86_64,
    Aarch64,
}
#[derive(Clone, Copy, Debug)]
pub struct Host {
    pub os: HostOs,
    pub arch: HostArch,
}

impl Host {
    pub fn current() -> Result<Self> {
        let os = current_os()?;
        let arch = current_arch()?;
        Ok(Self { os, arch })
    }
    fn object_arch(self) -> Architecture {
        match self.arch {
            HostArch::X86_64 => Architecture::X86_64,
            HostArch::Aarch64 => Architecture::Aarch64,
        }
    }
}

#[cfg(target_os = "linux")]
fn current_os() -> Result<HostOs> {
    Ok(HostOs::Linux)
}

#[cfg(target_os = "macos")]
fn current_os() -> Result<HostOs> {
    Ok(HostOs::Macos)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn current_os() -> Result<HostOs> {
    anyhow::bail!("unsupported operating system")
}

#[cfg(target_arch = "x86_64")]
fn current_arch() -> Result<HostArch> {
    Ok(HostArch::X86_64)
}

#[cfg(target_arch = "aarch64")]
fn current_arch() -> Result<HostArch> {
    Ok(HostArch::Aarch64)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn current_arch() -> Result<HostArch> {
    anyhow::bail!("unsupported CPU architecture")
}

pub fn inspect(path: &Path, host: Host) -> Result<bool> {
    inspect_named(path, path, host)
}

fn inspect_named(path: &Path, command_path: &Path, host: Host) -> Result<bool> {
    if script(path, command_path)? {
        return Ok(true);
    }
    if shared_library_name(path) || shared_library_name(command_path) {
        return Ok(false);
    }
    let cache = ReadCache::new(File::open(path)?);
    let kind = match FileKind::parse(&cache) {
        Ok(k) => k,
        Err(_) => return Ok(false),
    };
    match kind {
        FileKind::MachOFat32 if host.os == HostOs::Macos => fat32(&cache, host),
        FileKind::MachOFat64 if host.os == HostOs::Macos => fat64(&cache, host),
        _ => inspect_object(&cache, host),
    }
}

fn inspect_object<'a, R: object::read::ReadRef<'a>>(data: R, host: Host) -> Result<bool> {
    let file = match object::File::parse(data) {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };
    let expected = match host.os {
        HostOs::Linux => BinaryFormat::Elf,
        HostOs::Macos => BinaryFormat::MachO,
    };
    if file.format() != expected || file.architecture() != host.object_arch() || !file.is_64() {
        return Ok(false);
    }
    match host.os {
        HostOs::Macos => Ok(file.kind() == ObjectKind::Executable),
        HostOs::Linux => {
            let os_abi = match &file {
                object::File::Elf64(elf) => elf.elf_header().e_ident.os_abi,
                _ => return Ok(false),
            };
            Ok(linux_elf_os_abi(os_abi)
                && matches!(file.kind(), ObjectKind::Executable | ObjectKind::Dynamic)
                && file.entry() != 0)
        }
    }
}

fn linux_elf_os_abi(os_abi: u8) -> bool {
    matches!(
        os_abi,
        object::elf::ELFOSABI_SYSV | object::elf::ELFOSABI_GNU
    )
}

fn shared_library_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    name.ends_with(".so") || name.contains(".so.") || name.ends_with(".dylib")
}

fn elf_interpreter<'data, Elf, R>(file: &ElfFile<'data, Elf, R>) -> Result<Option<String>>
where
    Elf: FileHeader,
    R: ReadRef<'data>,
{
    for segment in file.elf_program_headers() {
        if let Some(interpreter) = segment.interpreter(file.endian(), file.data())? {
            return Ok(Some(std::str::from_utf8(interpreter)?.to_owned()));
        }
    }
    Ok(None)
}

/// Return the exact ELF `PT_INTERP` path, if the file has one.
pub fn elf_interpreter_path(path: &Path) -> Result<Option<String>> {
    let cache = ReadCache::new(File::open(path)?);
    if FileKind::parse(&cache)? != FileKind::Elf64 {
        return Ok(None);
    }
    let file = ElfFile64::<object::Endianness, _>::parse(&cache)?;
    elf_interpreter(&file)
}

fn fat32(cache: &ReadCache<File>, host: Host) -> Result<bool> {
    let fat = MachOFatFile32::parse(cache)?;
    for arch in fat.arches() {
        if arch.architecture() == host.object_arch() && inspect_object(arch.data(cache)?, host)? {
            return Ok(true);
        }
    }
    Ok(false)
}
fn fat64(cache: &ReadCache<File>, host: Host) -> Result<bool> {
    let fat = MachOFatFile64::parse(cache)?;
    for arch in fat.arches() {
        if arch.architecture() == host.object_arch() && inspect_object(arch.data(cache)?, host)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn script(path: &Path, command_path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut buf = [0u8; 512];
    let n = file.read(&mut buf)?;
    let bytes = &buf[..n];
    if !bytes.starts_with(b"#!") {
        return Ok(false);
    }
    if command_path.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("install")
    }) {
        return Ok(false);
    }
    let line = bytes[2..].split(|b| *b == b'\n').next().unwrap_or(&[]);
    let line = std::str::from_utf8(line)?.trim();
    if !line.starts_with('/') {
        return Ok(false);
    }
    let mut words = line.split_whitespace();
    let interpreter = words.next().unwrap_or("");
    if interpreter == "/usr/bin/env" {
        return Ok(words.next().is_some());
    }
    Ok(Path::new(interpreter).is_absolute())
}

pub fn executable_candidates(
    root: &Path,
    archive_has_modes: bool,
    host: Host,
) -> Result<Vec<PathBuf>> {
    let root = root.canonicalize()?;
    let search_root = if root.join("bin").is_dir() {
        root.join("bin")
    } else {
        root.clone()
    };
    let mut out = Vec::new();
    for entry in fs::read_dir(search_root)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            let resolved = path
                .canonicalize()
                .with_context(|| format!("resolve {}", path.display()))?;
            if !resolved.starts_with(&root) {
                continue;
            }
            if archive_has_modes && fs::metadata(&resolved)?.permissions().mode() & 0o111 == 0 {
                continue;
            }
            if inspect_named(&resolved, &path, host)? {
                out.push(path)
            }
        } else if meta.is_file() {
            if archive_has_modes && meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
            if inspect(&path, host)? {
                out.push(path)
            }
        }
    }
    out.sort();
    Ok(out)
}

pub fn descend_single_root(mut root: PathBuf) -> Result<PathBuf> {
    loop {
        let mut dirs = Vec::new();
        let mut blockers = 0;
        for item in fs::read_dir(&root)? {
            let item = item?;
            let p = item.path();
            if p.is_dir() {
                dirs.push(p)
            } else if !is_doc(&p) {
                blockers += 1
            }
        }
        if blockers > 0 || dirs.len() != 1 || dirs[0].file_name().is_some_and(|n| n == "bin") {
            return Ok(root);
        }
        root = dirs.pop().unwrap();
    }
}
fn is_doc(p: &Path) -> bool {
    let n = p
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    n == "install"
        || n.contains("readme")
        || n.contains("license")
        || n.contains("changelog")
        || [".txt", ".md", ".rst"].iter().any(|x| n.ends_with(x))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn scripts() {
        let t = tempfile::NamedTempFile::new().unwrap();
        writeln!(t.as_file(), "#!/usr/bin/env sh\necho hi").unwrap();
        assert!(
            inspect(
                t.path(),
                Host {
                    os: HostOs::Linux,
                    arch: HostArch::X86_64
                }
            )
            .unwrap()
        )
    }

    #[test]
    fn linux_elf_os_abi_rejects_other_unices() {
        assert!(linux_elf_os_abi(object::elf::ELFOSABI_SYSV));
        assert!(linux_elf_os_abi(object::elf::ELFOSABI_GNU));
        for os_abi in [
            object::elf::ELFOSABI_NETBSD,
            object::elf::ELFOSABI_SOLARIS,
            object::elf::ELFOSABI_FREEBSD,
            object::elf::ELFOSABI_OPENBSD,
        ] {
            assert!(!linux_elf_os_abi(os_abi));
        }
    }

    fn write_script(path: &Path) {
        fs::write(path, "#!/usr/bin/env sh\necho hi\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn copy_executable_fixture(path: &Path, fixture: &str) {
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/executables")
                .join(fixture),
            path,
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn shared_library_names_are_filtered_without_affecting_scripts() {
        for name in [
            "extension.so",
            "extension.SO",
            "libz.so.1",
            "libz.SO.1",
            "library.dylib",
            "library.DYLIB",
        ] {
            assert!(shared_library_name(Path::new(name)), "{name}");
        }
        for name in ["tool.software", "tool.dylib-helper", "tool.bundle"] {
            assert!(!shared_library_name(Path::new(name)), "{name}");
        }

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("command.so");
        write_script(&script);
        assert!(
            inspect(
                &script,
                Host {
                    os: HostOs::Linux,
                    arch: HostArch::X86_64,
                }
            )
            .unwrap()
        );
    }

    #[test]
    fn aws_shaped_tree_exposes_commands_but_not_shared_libraries() {
        let host = Host {
            os: HostOs::Linux,
            arch: HostArch::X86_64,
        };
        let temp = tempfile::tempdir().unwrap();
        for command in ["aws", "aws_completer"] {
            copy_executable_fixture(&temp.path().join(command), "elf-x86_64-exec");
        }
        for library in ["_awscrt.abi3.so", "libz.so.1"] {
            copy_executable_fixture(&temp.path().join(library), "elf-x86_64-pie");
        }
        symlink("libz.so.1", temp.path().join("hidden-library-name")).unwrap();

        let root = temp.path().canonicalize().unwrap();
        assert_eq!(
            executable_candidates(temp.path(), true, host).unwrap(),
            vec![root.join("aws"), root.join("aws_completer")]
        );
    }

    #[test]
    fn binary_discovery_is_one_directory_and_prefers_bin() {
        let host = Host {
            os: HostOs::Linux,
            arch: HostArch::X86_64,
        };
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        write_script(&temp.path().join("nested/tool"));
        assert!(
            executable_candidates(temp.path(), true, host)
                .unwrap()
                .is_empty()
        );

        write_script(&temp.path().join("install-helper"));
        assert!(
            executable_candidates(temp.path(), true, host)
                .unwrap()
                .is_empty()
        );

        fs::create_dir(temp.path().join("bin")).unwrap();
        write_script(&temp.path().join("bin/tool"));
        write_script(&temp.path().join("ignored-at-root"));
        assert_eq!(
            executable_candidates(temp.path(), true, host).unwrap(),
            vec![temp.path().canonicalize().unwrap().join("bin/tool")]
        );
    }
}
