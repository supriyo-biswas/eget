use eget::archive;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

const EXPECTED_HASH: &str = "1cd4ddb965ab4da643b75218824acf3407ce83824400f2a575a4265a9658ddc0";

fn fixtures() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/archives")
        .leak()
}

#[test]
fn safe_archive_families_preserve_layout_and_metadata() {
    for name in [
        "safe.7z",
        "safe.tar",
        "safe.tar.gz",
        "safe.tar.bz2",
        "safe.tar.xz",
        "safe.tar.zst",
        "safe.zip",
    ] {
        let temp = tempfile::tempdir().unwrap();
        archive::extract(&fixtures().join(name), name, "tool", temp.path()).unwrap();
        let root = temp.path().join("package");
        assert!(root.join("empty").is_dir(), "{name}");
        assert_eq!(
            fs::read_link(root.join("bin/tool-link")).unwrap(),
            Path::new("tool"),
            "{name}"
        );
        assert_eq!(
            fs::metadata(root.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "{name}"
        );
        assert_eq!(
            fs::metadata(root.join("share/data.bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640,
            "{name}"
        );
        assert_eq!(
            hex::encode(Sha256::digest(
                fs::read(root.join("share/data.bin")).unwrap()
            )),
            EXPECTED_HASH,
            "{name}"
        );
        if !matches!(name, "safe.7z" | "safe.zip") {
            assert_eq!(
                fs::metadata(root.join("share/data.bin")).unwrap().ino(),
                fs::metadata(root.join("share/data-hard")).unwrap().ino(),
                "{name}"
            );
        }
    }
}

#[test]
fn malicious_archives_are_rejected_without_outside_writes() {
    for name in [
        "symlink-escape.7z",
        "absolute.tar",
        "dotdot.tar",
        "symlink-child.tar",
        "hardlink-escape.tar",
        "fifo.tar",
        "conflicting-duplicate.tar",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().parent().unwrap().join("eget-fixture-outside");
        let _ = fs::remove_file(&outside);
        assert!(
            archive::extract(&fixtures().join(name), name, "tool", temp.path()).is_err(),
            "{name}"
        );
        assert!(!outside.exists(), "{name}");
    }
}

#[test]
fn encrypted_7z_is_rejected_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        archive::extract(
            &fixtures().join("encrypted.7z"),
            "encrypted.7z",
            "tool",
            temp.path(),
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn single_stream_formats_become_executable() {
    for name in ["single.gz", "single.bz2", "single.xz", "single.zst"] {
        let temp = tempfile::tempdir().unwrap();
        archive::extract(&fixtures().join(name), name, "tool", temp.path()).unwrap();
        assert_eq!(
            fs::metadata(temp.path().join("tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}
