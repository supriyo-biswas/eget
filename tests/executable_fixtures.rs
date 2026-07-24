use eget::compat::{self, Host, HostArch, HostOs};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/executables")
        .join(name)
}
fn linux(arch: HostArch) -> Host {
    Host {
        os: HostOs::Linux,
        arch,
    }
}
fn macos(arch: HostArch) -> Host {
    Host {
        os: HostOs::Macos,
        arch,
    }
}

#[test]
fn elf_format_architecture_and_runnable_kind_are_enforced() {
    assert!(compat::inspect(&fixture("elf-x86_64-exec"), linux(HostArch::X86_64)).unwrap());
    assert!(compat::inspect(&fixture("elf-aarch64-exec"), linux(HostArch::Aarch64)).unwrap());
    assert!(compat::inspect(&fixture("elf-x86_64-pie"), linux(HostArch::X86_64)).unwrap());
    assert!(compat::inspect(&fixture("elf-x86_64-static-pie"), linux(HostArch::X86_64)).unwrap());
    assert!(!compat::inspect(&fixture("elf-x86_64-shared"), linux(HostArch::X86_64)).unwrap());
    assert!(!compat::inspect(&fixture("elf-x86_64-freebsd"), linux(HostArch::X86_64)).unwrap());
    assert!(
        compat::inspect(
            &fixture("elf-x86_64-missing-loader"),
            linux(HostArch::X86_64)
        )
        .unwrap()
    );
    assert!(!compat::inspect(&fixture("elf-aarch64-exec"), linux(HostArch::X86_64)).unwrap());
    assert_eq!(
        compat::elf_interpreter_path(&fixture("elf-x86_64-missing-loader"))
            .unwrap()
            .as_deref(),
        Some("/definitely/missing/ld.so")
    );
}

#[test]
fn thin_and_universal_macho_require_executable_arm64_slices() {
    assert!(compat::inspect(&fixture("macho-arm64-exec"), macos(HostArch::Aarch64)).unwrap());
    assert!(!compat::inspect(&fixture("macho-arm64-dylib"), macos(HostArch::Aarch64)).unwrap());
    assert!(compat::inspect(&fixture("macho-universal-exec"), macos(HostArch::Aarch64)).unwrap());
    assert!(!compat::inspect(&fixture("macho-universal-exec"), macos(HostArch::X86_64)).unwrap());
}

#[test]
fn valid_shebang_forms_are_portable() {
    for name in ["script-env", "script-absolute"] {
        assert!(compat::inspect(&fixture(name), linux(HostArch::Aarch64)).unwrap());
    }
}
