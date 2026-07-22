use eget::db::{self, Database};
use eget::model::{HttpValidators, PackageId, PackageRecord, SourceKind};
use eget::scope::Scope;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct ExecFixture {
    temp: tempfile::TempDir,
    scope: Scope,
}

impl ExecFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let scope = Scope::from_paths(
            temp.path().join("packages"),
            temp.path().join("data/eget"),
            temp.path().join("bin"),
        );
        scope.prepare().unwrap();
        Self { temp, scope }
    }

    fn add_package(&self, id: &str, bin_dir: &Path, command: &str, script: &str) -> PathBuf {
        let id = PackageId::parse(id).unwrap();
        let (owner, app) = id.parts().unwrap();
        let owner = owner.to_owned();
        let app = app.to_owned();
        let installation_dir = self.scope.installation_dir(&id);
        fs::create_dir_all(&installation_dir).unwrap();
        fs::create_dir_all(bin_dir).unwrap();
        let executable = installation_dir.join(command);
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let link = bin_dir.join(command);
        symlink(&executable, &link).unwrap();

        let package = PackageRecord {
            id,
            current_version: None,
            owner,
            app,
            source_kind: SourceKind::Direct,
            installation_dir,
            bin_dir: bin_dir.to_owned(),
            pinned: true,
            installed_asset_url: "https://example.com/tool".into(),
            channel: None,
            release_selector: None,
            version_check_url: None,
            validators: HttpValidators::default(),
            rename_rules: Vec::new(),
            installed_at: "2026-07-23T00:00:00.000Z".into(),
            updated_at: None,
            binaries: vec![command.into()],
        };
        let mut database = Database::open(&self.scope.database, &self.scope.package_root).unwrap();
        let transaction = database.transaction().unwrap();
        db::replace_package(&transaction, &package).unwrap();
        transaction.commit().unwrap();
        link
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_eget"));
        command
            .arg("--scope=local")
            .env("EGET_LOCAL_DATA_DIR", self.temp.path().join("data"))
            .env("EGET_LOCAL_LOCK_DIR", self.temp.path().join("data/eget"))
            .env("EGET_LOCAL_PKG_DIR", &self.scope.package_root)
            .env("EGET_LOCAL_BIN_DIR", &self.scope.bin_dir)
            .env_remove("EGET_BIN_DIR")
            .env_remove("EGET_BIN");
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command().args(arguments).output().unwrap()
    }
}

#[test]
fn exec_replaces_eget_and_inherits_arguments_environment_and_working_directory() {
    let fixture = ExecFixture::new();
    let working_directory = fixture.temp.path().join("work");
    fs::create_dir(&working_directory).unwrap();
    fixture.add_package(
        "example.com/alpha",
        &fixture.scope.bin_dir,
        "tool",
        "#!/bin/sh\nprintf '%s|%s|%s' \"$1\" \"$EXEC_MARKER\" \"$PWD\"\nexit 37\n",
    );

    let output = fixture
        .command()
        .current_dir(&working_directory)
        .env("EXEC_MARKER", "inherited")
        .args(["x", "tool", "--flag"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(37));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("--flag|inherited|{}", working_directory.display())
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn ambiguity_lists_sorted_packages_and_exact_package_selects_one() {
    let fixture = ExecFixture::new();
    let first_bin = fixture.temp.path().join("first-bin");
    let second_bin = fixture.temp.path().join("second-bin");
    fixture.add_package(
        "example.com/zeta",
        &first_bin,
        "tool",
        "#!/bin/sh\necho zeta\n",
    );
    fixture.add_package(
        "example.com/alpha",
        &second_bin,
        "tool",
        "#!/bin/sh\necho alpha\n",
    );

    let ambiguous = fixture.run(&["exec", "tool"]);
    assert!(!ambiguous.status.success());
    assert!(ambiguous.stdout.is_empty());
    let error = String::from_utf8(ambiguous.stderr).unwrap();
    let alpha = error.find("example.com/alpha").unwrap();
    let zeta = error.find("example.com/zeta").unwrap();
    assert!(alpha < zeta, "{error}");
    assert!(error.contains("rerun with: eget x -p <PACKAGE_ID> tool"));

    let selected = fixture.run(&["x", "-p", "example.com/zeta", "tool"]);
    assert!(selected.status.success());
    assert_eq!(selected.stdout, b"zeta\n");
    assert!(selected.stderr.is_empty());
}

#[test]
fn exec_reports_missing_commands_and_package_mismatches() {
    let fixture = ExecFixture::new();
    fixture.add_package(
        "example.com/alpha",
        &fixture.scope.bin_dir,
        "tool",
        "#!/bin/sh\nexit 0\n",
    );

    let missing = fixture.run(&["x", "missing"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8(missing.stderr)
            .unwrap()
            .contains("command not installed in active scope: missing")
    );

    let mismatch = fixture.run(&["x", "-p", "example.com/alpha", "missing"]);
    assert!(!mismatch.status.success());
    assert!(
        String::from_utf8(mismatch.stderr)
            .unwrap()
            .contains("package example.com/alpha does not provide command \"missing\"")
    );

    let prefix = fixture.run(&["x", "-p", "example.com/al", "tool"]);
    assert!(!prefix.status.success());
    assert!(
        String::from_utf8(prefix.stderr)
            .unwrap()
            .contains("package ID not installed: example.com/al")
    );
}

#[test]
fn exec_rejects_unavailable_or_unowned_command_links() {
    let fixture = ExecFixture::new();
    let link = fixture.add_package(
        "example.com/alpha",
        &fixture.scope.bin_dir,
        "tool",
        "#!/bin/sh\nexit 0\n",
    );

    fs::remove_file(&link).unwrap();
    let missing = fixture.run(&["x", "tool"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8(missing.stderr)
            .unwrap()
            .contains("managed command link is unavailable")
    );

    fs::write(&link, "not a symlink").unwrap();
    let replaced = fixture.run(&["x", "tool"]);
    assert!(!replaced.status.success());
    assert!(
        String::from_utf8(replaced.stderr)
            .unwrap()
            .contains("managed command link is not a symlink")
    );

    fs::remove_file(&link).unwrap();
    symlink(fixture.temp.path().join("missing"), &link).unwrap();
    let broken = fixture.run(&["x", "tool"]);
    assert!(!broken.status.success());
    assert!(
        String::from_utf8(broken.stderr)
            .unwrap()
            .contains("managed command link is broken")
    );

    fs::remove_file(&link).unwrap();
    let outside = fixture.temp.path().join("outside");
    fs::write(&outside, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&outside, &link).unwrap();
    let redirected = fixture.run(&["x", "tool"]);
    assert!(!redirected.status.success());
    assert!(
        String::from_utf8(redirected.stderr)
            .unwrap()
            .contains("managed command link resolves outside package example.com/alpha")
    );
}
