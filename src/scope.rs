use crate::model::PackageId;
use anyhow::{Context, Result, bail};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeKind {
    System,
    User,
    Local,
}

impl FromStr for ScopeKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            "local" => Ok(Self::Local),
            _ => bail!("invalid scope {value:?}; expected system, user, or local"),
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
}

impl Scope {
    pub fn detect(requested: Option<ScopeKind>, destination: Option<PathBuf>) -> Result<Self> {
        let is_root = unsafe { libc::geteuid() } == 0;
        let environment_scope = env::var("EGET_SCOPE")
            .ok()
            .map(|value| value.parse())
            .transpose()?;
        Self::resolve(
            requested.or(environment_scope),
            destination,
            is_root,
            |name| env::var_os(name),
        )
    }

    fn resolve(
        requested: Option<ScopeKind>,
        destination: Option<PathBuf>,
        is_root: bool,
        environment: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self> {
        let kind = requested.unwrap_or(if is_root {
            ScopeKind::System
        } else {
            ScopeKind::User
        });
        if kind == ScopeKind::System && !is_root {
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
                })
            }
            ScopeKind::Local => {
                if destination.is_some()
                    || environment("EGET_BIN_DIR").is_some()
                    || environment("EGET_BIN").is_some()
                {
                    bail!("binary-directory overrides are not allowed in local scope")
                }
                let required = |name| {
                    environment(name)
                        .map(PathBuf::from)
                        .with_context(|| format!("{name} is required in local scope"))
                };
                let state = required("EGET_LOCAL_DATA_DIR")?.join("eget");
                Ok(Self {
                    kind,
                    package_root: required("EGET_LOCAL_PKG_DIR")?,
                    database: state.join("eget.sqlite3"),
                    lock: required("EGET_LOCAL_LOCK_DIR")?.join("eget.lock"),
                    bin_dir: required("EGET_LOCAL_BIN_DIR")?,
                    legacy_database: None,
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

    #[doc(hidden)]
    pub fn from_paths(package_root: PathBuf, state_root: PathBuf, bin_dir: PathBuf) -> Self {
        Self {
            kind: ScopeKind::Local,
            package_root,
            database: state_root.join("eget.sqlite3"),
            lock: state_root.join("eget.lock"),
            bin_dir,
            legacy_database: None,
        }
    }
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

    fn resolve(
        requested: Option<ScopeKind>,
        is_root: bool,
        values: &[(&str, &str)],
    ) -> Result<Scope> {
        let values = values.iter().copied().collect::<HashMap<_, _>>();
        Scope::resolve(requested, None, is_root, |name| {
            values.get(name).map(OsString::from)
        })
    }

    #[test]
    fn defaults_and_xdg_fallbacks_match_contract() {
        let user = resolve(None, false, &[("HOME", "/home/test")]).unwrap();
        assert_eq!(user.kind, ScopeKind::User);
        assert_eq!(user.package_root, Path::new("/home/test/.local/share/eget"));
        assert_eq!(
            user.database,
            Path::new("/home/test/.local/share/eget/eget.sqlite3")
        );
        assert_eq!(user.lock, Path::new("/home/test/.local/share/eget.lock"));

        let system = resolve(None, true, &[]).unwrap();
        assert_eq!(
            system.database,
            Path::new("/var/lib/eget/eget/eget.sqlite3")
        );
        assert_eq!(system.lock, Path::new("/run/lock/eget.lock"));
    }

    #[test]
    fn local_scope_requires_every_path_and_rejects_bin_override() {
        assert!(resolve(Some(ScopeKind::Local), false, &[]).is_err());
        let local = resolve(
            Some(ScopeKind::Local),
            false,
            &[
                ("EGET_LOCAL_DATA_DIR", "/state"),
                ("EGET_LOCAL_LOCK_DIR", "/lock"),
                ("EGET_LOCAL_PKG_DIR", "/pkg"),
                ("EGET_LOCAL_BIN_DIR", "/bin"),
            ],
        )
        .unwrap();
        assert_eq!(local.database, Path::new("/state/eget/eget.sqlite3"));
    }

    #[test]
    fn non_root_cannot_request_system_scope() {
        assert!(resolve(Some(ScopeKind::System), false, &[("HOME", "/tmp")]).is_err());
    }
}
