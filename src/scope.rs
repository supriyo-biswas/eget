use crate::model::PackageId;
use anyhow::{Context, Result, bail};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeKind {
    System,
    User,
    Project,
}

impl FromStr for ScopeKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            _ => bail!("invalid scope {value:?}; expected system, user, or project"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub package_root: PathBuf,
    pub database: PathBuf,
    pub lock: PathBuf,
    pub bin_dir: PathBuf,
    pub legacy_database: Option<PathBuf>,
    project_root: Option<PathBuf>,
    manifest: Option<PathBuf>,
}

impl Scope {
    pub fn detect(requested: Option<ScopeKind>, destination: Option<PathBuf>) -> Result<Self> {
        let effective_uid = unsafe { libc::geteuid() };
        let environment_scope = env::var("EGET_SCOPE")
            .ok()
            .map(|value| value.parse())
            .transpose()?;
        let requested = requested.or(environment_scope);
        let project_root = if requested == Some(ScopeKind::Project)
            || (requested.is_none() && effective_uid != 0)
        {
            let home = env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set")?;
            let current = env::current_dir().context("get current directory")?;
            find_project_root(&current, &home, effective_uid, |path| {
                Ok(fs::metadata(path)?.uid())
            })?
        } else {
            None
        };
        Self::resolve(
            requested,
            destination,
            effective_uid,
            project_root,
            |name| env::var_os(name),
        )
    }

    fn resolve(
        requested: Option<ScopeKind>,
        destination: Option<PathBuf>,
        effective_uid: u32,
        project_root: Option<PathBuf>,
        environment: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self> {
        let kind = requested.unwrap_or_else(|| {
            if effective_uid == 0 {
                ScopeKind::System
            } else if project_root.is_some() {
                ScopeKind::Project
            } else {
                ScopeKind::User
            }
        });
        if kind == ScopeKind::System && effective_uid != 0 {
            bail!("system scope requires root privileges")
        }

        let override_bin = || {
            destination.clone().or_else(|| {
                environment("EGET_BIN_DIR")
                    .or_else(|| environment("EGET_BIN"))
                    .map(PathBuf::from)
            })
        };
        match kind {
            ScopeKind::System => {
                let state = PathBuf::from("/var/lib/eget").join("eget");
                Ok(Self {
                    kind,
                    package_root: PathBuf::from("/opt/eget"),
                    database: state.join("eget.sqlite3"),
                    lock: PathBuf::from("/run/lock/eget.lock"),
                    bin_dir: override_bin().unwrap_or_else(|| PathBuf::from("/usr/local/bin")),
                    legacy_database: Some(PathBuf::from("/var/opt/eget/eget.sqlite3")),
                    project_root: None,
                    manifest: None,
                })
            }
            ScopeKind::User => {
                let home = environment("HOME")
                    .map(PathBuf::from)
                    .context("HOME is not set")?;
                let data = environment("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/share"));
                let runtime = environment("XDG_RUNTIME_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| data.clone());
                let state = data.join("eget");
                let legacy_state = environment("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/state"))
                    .join("eget/eget.sqlite3");
                Ok(Self {
                    kind,
                    package_root: state.clone(),
                    database: state.join("eget.sqlite3"),
                    lock: runtime.join("eget.lock"),
                    bin_dir: override_bin().unwrap_or_else(|| home.join(".local/bin")),
                    legacy_database: Some(legacy_state),
                    project_root: None,
                    manifest: None,
                })
            }
            ScopeKind::Project => {
                if destination.is_some()
                    || environment("EGET_BIN_DIR").is_some()
                    || environment("EGET_BIN").is_some()
                {
                    bail!("binary-directory overrides are not allowed in project scope")
                }
                let project_root = project_root.context(
                    "project scope requires an eget-packages.txt in an owned directory below HOME",
                )?;
                let state = project_root.join(".eget");
                Ok(Self {
                    kind,
                    package_root: state.clone(),
                    database: state.join("eget.sqlite3"),
                    lock: state.join("eget.lock"),
                    bin_dir: state.join("bin"),
                    legacy_database: None,
                    manifest: Some(project_root.join("eget-packages.txt")),
                    project_root: Some(project_root),
                })
            }
        }
    }

    pub fn prepare(&self) -> Result<()> {
        create_private_dir(&self.package_root)?;
        if let Some(parent) = self.database.parent() {
            create_private_dir(parent)?;
        }
        if let Some(parent) = self.lock.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create lock directory {}", parent.display()))?;
        }
        fs::create_dir_all(&self.bin_dir)
            .with_context(|| format!("create binary directory {}", self.bin_dir.display()))?;
        Ok(())
    }

    pub fn installation_dir(&self, id: &PackageId) -> PathBuf {
        self.package_root.join(id.directory_name())
    }

    pub fn validate_install_dir(&self, path: &Path) -> Result<()> {
        let root = self.package_root.canonicalize()?;
        let parent = path
            .parent()
            .context("package directory has no parent")?
            .canonicalize()?;
        if path == self.package_root || parent != root {
            bail!("unsafe package installation directory {}", path.display())
        }
        Ok(())
    }

    pub fn description(&self) -> String {
        match self.kind {
            ScopeKind::System => "system scope".into(),
            ScopeKind::User => "user scope".into(),
            ScopeKind::Project => {
                let root = self
                    .project_root
                    .as_deref()
                    .unwrap_or(self.package_root.as_path());
                format!("project scope ({})", display_path(root))
            }
        }
    }

    pub fn manifest(&self) -> Option<&Path> {
        self.manifest.as_deref()
    }

    #[doc(hidden)]
    pub fn from_paths(package_root: PathBuf, state_root: PathBuf, bin_dir: PathBuf) -> Self {
        let project_root = package_root.parent().map(Path::to_path_buf);
        Self {
            kind: ScopeKind::Project,
            package_root,
            database: state_root.join("eget.sqlite3"),
            lock: state_root.join("eget.lock"),
            bin_dir,
            legacy_database: None,
            project_root,
            manifest: None,
        }
    }
}

fn find_project_root(
    start: &Path,
    home: &Path,
    effective_uid: u32,
    owner: impl Fn(&Path) -> Result<u32>,
) -> Result<Option<PathBuf>> {
    let mut directory = start
        .canonicalize()
        .with_context(|| format!("resolve current directory {}", start.display()))?;
    let home = home
        .canonicalize()
        .with_context(|| format!("resolve HOME {}", home.display()))?;
    loop {
        if directory == home || owner(&directory)? != effective_uid {
            return Ok(None);
        }
        let marker = directory.join("eget-packages.txt");
        match fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.file_type().is_file() => return Ok(Some(directory)),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", marker.display()));
            }
        }
        if !directory.pop() {
            return Ok(None);
        }
    }
}

fn display_path(path: &Path) -> String {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| home.canonicalize().ok());
    display_path_with_home(path, home.as_deref())
}

fn display_path_with_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(relative) = home.and_then(|home| path.strip_prefix(home).ok()) {
        if relative.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set private permissions on {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    fn resolve(
        requested: Option<ScopeKind>,
        effective_uid: u32,
        project_root: Option<PathBuf>,
        values: &[(&str, &str)],
    ) -> Result<Scope> {
        let values = values.iter().copied().collect::<HashMap<_, _>>();
        Scope::resolve(requested, None, effective_uid, project_root, |name| {
            values.get(name).map(OsString::from)
        })
    }

    #[test]
    fn defaults_and_xdg_fallbacks_match_contract() {
        let user = resolve(None, 1000, None, &[("HOME", "/home/test")]).unwrap();
        assert_eq!(user.kind, ScopeKind::User);
        assert_eq!(user.package_root, Path::new("/home/test/.local/share/eget"));
        assert_eq!(
            user.database,
            Path::new("/home/test/.local/share/eget/eget.sqlite3")
        );
        assert_eq!(user.lock, Path::new("/home/test/.local/share/eget.lock"));

        let system = resolve(None, 0, None, &[]).unwrap();
        assert_eq!(
            system.database,
            Path::new("/var/lib/eget/eget/eget.sqlite3")
        );
        assert_eq!(system.lock, Path::new("/run/lock/eget.lock"));
    }

    #[test]
    fn scope_names_accept_project_without_a_local_alias() {
        assert_eq!("project".parse::<ScopeKind>().unwrap(), ScopeKind::Project);
        assert!("local".parse::<ScopeKind>().is_err());
    }

    #[test]
    fn project_scope_uses_project_eget_directory() {
        assert!(resolve(Some(ScopeKind::Project), 1000, None, &[]).is_err());
        let project = resolve(
            Some(ScopeKind::Project),
            1000,
            Some(PathBuf::from("/work/project")),
            &[],
        )
        .unwrap();
        assert_eq!(project.package_root, Path::new("/work/project/.eget"));
        assert_eq!(
            project.database,
            Path::new("/work/project/.eget/eget.sqlite3")
        );
        assert_eq!(project.lock, Path::new("/work/project/.eget/eget.lock"));
        assert_eq!(project.bin_dir, Path::new("/work/project/.eget/bin"));
    }

    #[test]
    fn automatic_and_explicit_scope_selection_follow_precedence() {
        let project_root = Some(PathBuf::from("/work/project"));
        assert_eq!(
            resolve(None, 1000, project_root.clone(), &[("HOME", "/home/test")])
                .unwrap()
                .kind,
            ScopeKind::Project
        );
        assert_eq!(
            resolve(
                Some(ScopeKind::User),
                1000,
                project_root.clone(),
                &[("HOME", "/home/test")],
            )
            .unwrap()
            .kind,
            ScopeKind::User
        );
        assert_eq!(
            resolve(None, 0, project_root.clone(), &[]).unwrap().kind,
            ScopeKind::System
        );
        assert_eq!(
            resolve(Some(ScopeKind::Project), 0, project_root, &[])
                .unwrap()
                .kind,
            ScopeKind::Project
        );
    }

    #[test]
    fn project_scope_rejects_binary_directory_overrides() {
        let scope = Scope::resolve(
            Some(ScopeKind::Project),
            None,
            1000,
            Some(PathBuf::from("/work/project")),
            |name| (name == "EGET_BIN").then(|| OsString::from("/other/bin")),
        );
        assert!(scope.is_err());
    }

    #[test]
    fn non_root_cannot_request_system_scope() {
        assert!(resolve(Some(ScopeKind::System), 1000, None, &[("HOME", "/tmp")]).is_err());
    }

    #[test]
    fn project_discovery_uses_nearest_marker_and_ignores_home_marker() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = home.join("project");
        let nested = project.join("nested/work");
        fs::create_dir_all(&nested).unwrap();
        fs::write(home.join("eget-packages.txt"), "").unwrap();
        fs::write(project.join("eget-packages.txt"), "").unwrap();
        fs::write(project.join("nested/eget-packages.txt"), "").unwrap();

        let found = find_project_root(&nested, &home, 1000, |_| Ok(1000)).unwrap();
        assert_eq!(found, Some(project.join("nested").canonicalize().unwrap()));

        fs::remove_file(project.join("nested/eget-packages.txt")).unwrap();
        fs::remove_file(project.join("eget-packages.txt")).unwrap();
        assert_eq!(
            find_project_root(&nested, &home, 1000, |_| Ok(1000)).unwrap(),
            None
        );
    }

    #[test]
    fn project_discovery_stops_before_a_foreign_owned_directory() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("shared/project");
        let nested = project.join("nested");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(project.join("eget-packages.txt"), "").unwrap();
        let foreign = project.canonicalize().unwrap();

        let found = find_project_root(&nested, &home, 1000, |path| {
            Ok(if path == foreign { 2000 } else { 1000 })
        })
        .unwrap();
        assert_eq!(found, None);
    }

    #[test]
    fn project_scope_paths_use_home_relative_or_absolute_display() {
        assert_eq!(
            display_path_with_home(
                Path::new("/home/test/project"),
                Some(Path::new("/home/test")),
            ),
            "~/project"
        );
        assert_eq!(
            display_path_with_home(Path::new("/work/project"), Some(Path::new("/home/test"))),
            "/work/project"
        );
    }
}
