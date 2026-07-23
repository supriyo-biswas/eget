use crate::archive::{self, Format};
use crate::compat;
use crate::db::{self, Database};
use crate::model::{HttpValidators, PackageId, PackageRecord, RenameRule, SourceKind};
use crate::policy::Channel;
use crate::scope::Scope;
use crate::source::{self, AssetCandidate, ResolvedPackage};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, ETAG, LAST_MODIFIED};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use tempfile::{Builder, TempDir};

pub struct Installer {
    scope: Scope,
    client: Client,
}

#[derive(Clone, Debug, Default)]
pub struct InstallOptions {
    pub force: bool,
    pub pin: Option<bool>,
    pub channel: Option<Channel>,
    pub reinstall: bool,
    pub ignore_existing: bool,
    pub version_url: Option<String>,
    pub rename_rules: Vec<RenameRule>,
    pub relocate: bool,
}

impl Installer {
    pub fn new(scope: Scope) -> Result<Self> {
        Ok(Self {
            scope,
            client: source::client()?,
        })
    }

    pub fn install_many(&self, inputs: &[String], options: &InstallOptions) -> Result<u8> {
        if options.version_url.is_some() && inputs.len() != 1 {
            bail!("--version-url may only be used with one package")
        }
        self.session(|session| {
            let mut failed = false;
            for input in inputs {
                if let Err(error) = session.install(input, options) {
                    eprintln!("Error processing {input}: {error:#}");
                    failed = true;
                }
            }
            Ok(u8::from(failed))
        })
    }

    pub fn list(&self, filters: &[String]) -> Result<u8> {
        self.session(|session| {
            let packages = session.database.packages()?;
            for package in packages.iter().filter(|package| {
                filters.is_empty()
                    || filters.iter().any(|filter| {
                        package.id.as_str().starts_with(filter) || package.owner.starts_with(filter)
                    })
            }) {
                println!("{}", format_package(package));
            }
            Ok(0)
        })
    }

    pub fn mark_many(
        &self,
        ids: &[String],
        pin: Option<bool>,
        channel: Option<Channel>,
    ) -> Result<u8> {
        self.session(|session| {
            let mut failed = false;
            for id in ids {
                let result = (|| -> Result<()> {
                    let package = session
                        .database
                        .package(id)?
                        .with_context(|| format!("package ID not installed: {id}"))?;
                    if channel == Some(Channel::Prerelease)
                        && package.source_kind == SourceKind::Gitlab
                    {
                        bail!("GitLab packages do not support the prerelease channel")
                    }
                    let transaction = session.database.transaction()?;
                    db::mark_package(&transaction, id, pin, channel)?;
                    transaction.commit()?;
                    println!("Marked {id}");
                    Ok(())
                })();
                if let Err(error) = result {
                    eprintln!("Error processing {id}: {error:#}");
                    failed = true;
                }
            }
            Ok(u8::from(failed))
        })
    }

    pub fn uninstall_many(&self, ids: &[String]) -> Result<u8> {
        self.session(|session| {
            let mut failed = false;
            for id in ids {
                if let Err(error) = session.uninstall(id) {
                    eprintln!("Error processing {id}: {error:#}");
                    failed = true;
                }
            }
            Ok(u8::from(failed))
        })
    }

    pub(crate) fn executable(&self, command: &str, package_id: Option<&str>) -> Result<PathBuf> {
        self.session(|session| session.executable(command, package_id))
    }

    pub fn update_many(
        &self,
        requested_ids: &[String],
        confirm: impl FnOnce(usize) -> Result<bool>,
    ) -> Result<u8> {
        self.session(|session| {
            let ids = if requested_ids.is_empty() {
                session.database.package_ids()?
            } else {
                requested_ids.to_vec()
            };
            let progress = ProgressBar::new(ids.len() as u64);
            progress.set_style(
                ProgressStyle::with_template("Checking updates {wide_bar:.cyan/blue} {pos}/{len}")?
                    .progress_chars("=> "),
            );
            let mut pending = Vec::new();
            let mut skipped = Vec::new();
            let mut errors = Vec::new();
            let mut failed = false;
            for id in ids {
                match session.probe_update(&id) {
                    Ok(UpdateProbe::Available(update)) => pending.push(update),
                    Ok(UpdateProbe::Skipped(reason)) => skipped.push((id, reason)),
                    Ok(UpdateProbe::Unchanged) => {}
                    Err(error) => {
                        errors.push((id, error));
                        failed = true;
                    }
                }
                progress.inc(1);
            }
            progress.finish_and_clear();
            for (id, reason) in skipped {
                println!("Skipped {id}: {reason}");
            }
            for update in &pending {
                println!("{}", update.summary());
            }
            for (id, error) in errors {
                eprintln!("Error processing {id}: {error:#}");
            }
            if !pending.is_empty() && confirm(pending.len())? {
                for update in pending {
                    let id = update.installed.id.to_string();
                    if let Err(error) = session.apply_update(*update) {
                        eprintln!("Error processing {id}: {error:#}");
                        failed = true;
                    }
                }
            }
            Ok(u8::from(failed))
        })
    }

    fn session<T>(&self, operation: impl FnOnce(&mut Session<'_>) -> Result<T>) -> Result<T> {
        self.scope.prepare()?;
        let lock = open_lock(&self.scope.lock)?;
        acquire_lock(&lock)?;
        if let Some(legacy) = &self.scope.legacy_database {
            Database::reset_legacy_at(legacy, &self.scope.package_root)?;
        }
        let database = Database::open(&self.scope.database, &self.scope.package_root)?;
        let mut session = Session {
            scope: &self.scope,
            client: &self.client,
            database,
        };
        operation(&mut session)
    }
}

struct Session<'a> {
    scope: &'a Scope,
    client: &'a Client,
    database: Database,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PinNotice {
    Automatic,
    UnpinIgnored,
}

impl Session<'_> {
    fn executable(&self, command: &str, package_id: Option<&str>) -> Result<PathBuf> {
        let package = if let Some(id) = package_id {
            let package = self
                .database
                .package(id)?
                .with_context(|| format!("package ID not installed: {id}"))?;
            if !package.binaries.iter().any(|binary| binary == command) {
                bail!("package {id} does not provide command {command:?}")
            }
            package
        } else {
            let owners = self.database.owners_of_binary(command)?;
            let id = match owners.as_slice() {
                [] => bail!("command not installed in active scope: {command}"),
                [id] => id,
                _ => bail!(
                    "command {command:?} is provided by multiple packages:\n{}\n\
                     rerun with: eget x -p <PACKAGE_ID> {command}",
                    owners.join("\n")
                ),
            };
            self.database
                .package(id)?
                .with_context(|| format!("package ID not installed: {id}"))?
        };

        self.scope.validate_install_dir(&package.installation_dir)?;
        let installation_dir = package.installation_dir.canonicalize().with_context(|| {
            format!("resolve installation directory for package {}", package.id)
        })?;
        let link = package.bin_dir.join(command);
        let metadata = fs::symlink_metadata(&link)
            .with_context(|| format!("managed command link is unavailable: {}", link.display()))?;
        if !metadata.file_type().is_symlink() {
            bail!("managed command link is not a symlink: {}", link.display())
        }
        let target = link
            .canonicalize()
            .with_context(|| format!("managed command link is broken: {}", link.display()))?;
        if target == installation_dir || !target.starts_with(&installation_dir) {
            bail!(
                "managed command link resolves outside package {}: {}",
                package.id,
                link.display()
            )
        }
        if !target.is_file() {
            bail!("managed command target is not a file: {}", target.display())
        }
        Ok(target)
    }

    fn install(&mut self, input: &str, options: &InstallOptions) -> Result<()> {
        if options.version_url.is_some() && !input.contains("{{version}}") {
            bail!("--version-url requires the package URL to contain {{version}}")
        }
        let version_check = options
            .version_url
            .as_deref()
            .map(|url| fetch_version(self.client, url))
            .transpose()?;
        let version = version_check.as_ref().map(|(version, _)| version);
        let concrete_input = if let Some(version) = version {
            input.replace("{{version}}", version)
        } else {
            input.to_owned()
        };
        let requested_channel = options.channel.unwrap_or(Channel::Stable);
        let initial = source::resolve_with_store(
            self.client,
            &self.database,
            &concrete_input,
            requested_channel,
            None,
        )?;
        let installed = self.database.package(&initial.id)?;

        if options.ignore_existing && installed.is_some() {
            println!("Skipped {}: already installed", initial.id);
            return Ok(());
        }
        if installed.is_some()
            && !options.reinstall
            && (options.pin.is_some() || options.channel.is_some())
        {
            bail!("changing package policy requires --reinstall or `eget mark`")
        }

        let resolved = if let Some(installed) = &installed {
            if !options.reinstall {
                if installed.pinned {
                    println!("Skipped {}: pinned", installed.id);
                    return Ok(());
                }
                source::resolve_with_store(
                    self.client,
                    &self.database,
                    &concrete_input,
                    installed.channel.unwrap_or(Channel::Stable),
                    installed.release_selector.as_deref(),
                )?
            } else {
                initial
            }
        } else {
            initial
        };

        if let Some(installed) = &installed
            && !options.reinstall
            && !direct_changed(
                self.client,
                installed,
                &resolved,
                version.map(String::as_str),
            )?
        {
            println!("Unchanged {}", installed.id);
            return Ok(());
        }

        let mut prepared = prepare(self.client, self.scope, &resolved)?;
        let now = self.database.now()?;
        let version_check_url = options.version_url.clone().or_else(|| {
            installed
                .as_ref()
                .and_then(|package| package.version_check_url.clone())
        });
        let stored_validators = version_check
            .as_ref()
            .map(|(_, validators)| validators.clone())
            .unwrap_or_else(|| prepared.validators.clone());
        let (pinned, pin_notice) = pin_policy(
            effective_pin(
                options.pin,
                installed.as_ref().map(|package| package.pinned),
                options.reinstall,
                resolved.automatic_pin && version_check_url.is_none(),
            ),
            resolved.kind,
            version_check_url.as_deref(),
            &stored_validators,
            options.pin,
        );
        let channel = (resolved.kind != SourceKind::Direct).then_some(
            options
                .channel
                .or_else(|| installed.as_ref().and_then(|package| package.channel))
                .unwrap_or(resolved.channel),
        );
        let rename_rules = if options.rename_rules.is_empty() {
            installed
                .as_ref()
                .map(|package| package.rename_rules.clone())
                .unwrap_or_default()
        } else {
            options.rename_rules.clone()
        };
        prepared.apply_rename_rules(&rename_rules)?;
        let current_version = if resolved.kind == SourceKind::Direct {
            version.cloned()
        } else {
            resolved.tag.clone()
        };
        let record = PackageRecord {
            id: PackageId::parse(resolved.id.clone())?,
            current_version,
            owner: resolved.owner.clone(),
            app: resolved.app.clone(),
            source_kind: resolved.kind,
            installation_dir: self
                .scope
                .installation_dir(&PackageId::parse(resolved.id.clone())?),
            bin_dir: installed
                .as_ref()
                .filter(|_| !options.relocate)
                .map(|package| package.bin_dir.clone())
                .unwrap_or_else(|| self.scope.bin_dir.clone()),
            pinned,
            installed_asset_url: if options.version_url.is_some() {
                input.to_owned()
            } else {
                prepared.asset_url.clone()
            },
            channel,
            release_selector: resolved.release_selector.clone(),
            version_check_url,
            validators: stored_validators,
            rename_rules,
            installed_at: installed
                .as_ref()
                .map(|package| package.installed_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: installed.as_ref().map(|_| now),
            binaries: prepared.binary_names(),
        };
        self.activate(prepared, installed.as_ref(), record, options.force)?;
        println!(
            "{} {} in {}",
            if installed.is_some() {
                "Updated"
            } else {
                "Installed"
            },
            resolved.id,
            self.scope.description()
        );
        match pin_notice {
            Some(PinNotice::Automatic) => eprintln!(
                "Notice: pinned {} because the direct URL provides neither ETag nor Last-Modified",
                resolved.id
            ),
            Some(PinNotice::UnpinIgnored) => eprintln!(
                "Warning: could not unpin {} because the direct URL provides neither ETag nor Last-Modified",
                resolved.id
            ),
            None => {}
        }
        Ok(())
    }

    fn probe_update(&self, id: &str) -> Result<UpdateProbe> {
        let installed = self
            .database
            .package(id)?
            .with_context(|| package_not_found(&self.database, id))?;
        if installed.pinned {
            return Ok(UpdateProbe::Skipped("pinned"));
        }
        match installed.source_kind {
            SourceKind::Direct => self.probe_direct_update(installed),
            SourceKind::Github | SourceKind::Gitlab | SourceKind::Gitea => {
                let source = forge_source(&installed);
                let resolved = source::resolve_with_store(
                    self.client,
                    &self.database,
                    &source,
                    installed.channel.unwrap_or(Channel::Stable),
                    installed.release_selector.as_deref(),
                )?;
                if resolved.tag == installed.current_version {
                    Ok(UpdateProbe::Unchanged)
                } else {
                    Ok(UpdateProbe::Available(Box::new(PendingUpdate {
                        installed,
                        resolved,
                        version: None,
                        version_validators: None,
                    })))
                }
            }
        }
    }

    fn probe_direct_update(&self, installed: PackageRecord) -> Result<UpdateProbe> {
        if let Some(version_url) = &installed.version_check_url {
            let (version, version_validators) = fetch_version(self.client, version_url)?;
            if installed.current_version.as_deref() == Some(&version) {
                return Ok(UpdateProbe::Unchanged);
            }
            let concrete = installed
                .installed_asset_url
                .replace("{{version}}", &version);
            let resolved = source::resolve_with_preferences(
                self.client,
                &concrete,
                Some(SourceKind::Direct),
                Channel::Stable,
                None,
            )?;
            return Ok(UpdateProbe::Available(Box::new(PendingUpdate {
                installed,
                resolved,
                version: Some(version),
                version_validators: Some(version_validators),
            })));
        }
        if installed.validators.etag.is_none() && installed.validators.last_modified.is_none() {
            return Ok(UpdateProbe::Skipped("no HTTP validators"));
        }
        let response = source::conditional_head(
            self.client,
            &installed.installed_asset_url,
            installed.validators.etag.as_deref(),
            installed.validators.last_modified.as_deref(),
        )?;
        if response.status() == StatusCode::NOT_MODIFIED
            || validators(response.headers()) == installed.validators
        {
            return Ok(UpdateProbe::Unchanged);
        }
        let resolved = source::resolve_with_preferences(
            self.client,
            &installed.installed_asset_url,
            Some(SourceKind::Direct),
            Channel::Stable,
            None,
        )?;
        Ok(UpdateProbe::Available(Box::new(PendingUpdate {
            installed,
            resolved,
            version: None,
            version_validators: None,
        })))
    }

    fn apply_update(&mut self, update: PendingUpdate) -> Result<()> {
        let current = self
            .database
            .package(update.installed.id.as_str())?
            .context("package was removed while awaiting confirmation")?;
        if current != update.installed {
            bail!("package changed while awaiting confirmation")
        }
        let mut prepared = prepare(self.client, self.scope, &update.resolved)?;
        let now = self.database.now()?;
        let mut record = update.installed.clone();
        prepared.apply_rename_rules(&record.rename_rules)?;
        record.current_version = update.version.or(update.resolved.tag.clone());
        record.installed_asset_url = if record.version_check_url.is_some() {
            record.installed_asset_url.clone()
        } else {
            prepared.asset_url.clone()
        };
        record.validators = update
            .version_validators
            .unwrap_or_else(|| prepared.validators.clone());
        record.updated_at = Some(now);
        record.binaries = prepared.binary_names();
        self.activate(prepared, Some(&update.installed), record, false)?;
        println!("Updated {}", update.installed.id);
        Ok(())
    }

    fn activate(
        &mut self,
        prepared: Prepared,
        old: Option<&PackageRecord>,
        record: PackageRecord,
        force: bool,
    ) -> Result<()> {
        self.scope.validate_install_dir(&record.installation_dir)?;
        let links = prepared.links(&record.installation_dir, &record.bin_dir);
        let old_links = old
            .into_iter()
            .flat_map(|package| {
                package
                    .binaries
                    .iter()
                    .map(|name| package.bin_dir.join(name))
            })
            .collect::<BTreeSet<_>>();

        for (path, _) in &links {
            if fs::symlink_metadata(path).is_ok() && !old_links.contains(path) && !force {
                bail!(
                    "command path already exists: {} (use --force to replace it)",
                    path.display()
                )
            }
        }

        let backup_root = prepared.temp.path().join("rollback");
        fs::create_dir_all(&backup_root)?;
        let old_package = backup_root.join("package");
        if record.installation_dir.exists() {
            fs::rename(&record.installation_dir, &old_package)?;
        }

        let mut backed_up_links = BTreeMap::new();
        let mut all_link_paths = old_links;
        all_link_paths.extend(links.iter().map(|(path, _)| path.clone()));
        for (index, path) in all_link_paths.iter().enumerate() {
            if fs::symlink_metadata(path).is_ok() {
                let backup = backup_root.join(format!("link-{index}"));
                fs::rename(path, &backup)?;
                backed_up_links.insert(path.clone(), backup);
            }
        }

        let activation = (|| -> Result<()> {
            fs::rename(&prepared.root, &record.installation_dir)
                .context("promote staged package")?;
            fs::create_dir_all(&record.bin_dir)?;
            for (path, target) in &links {
                symlink(target, path).with_context(|| format!("link {}", path.display()))?;
            }
            let transaction = self.database.transaction()?;
            db::replace_package(&transaction, &record)?;
            transaction.commit()?;
            Ok(())
        })();

        if let Err(error) = activation {
            for (path, _) in &links {
                if fs::symlink_metadata(path).is_ok() {
                    let _ = fs::remove_file(path);
                }
            }
            if record.installation_dir.exists() {
                let _ = fs::rename(&record.installation_dir, &prepared.root);
            }
            if old_package.exists() {
                let _ = fs::rename(&old_package, &record.installation_dir);
            }
            for (path, backup) in backed_up_links {
                let _ = fs::rename(backup, path);
            }
            return Err(error);
        }
        Ok(())
    }

    fn uninstall(&mut self, id: &str) -> Result<()> {
        let package = self
            .database
            .package(id)?
            .with_context(|| package_not_found(&self.database, id))?;
        self.scope.validate_install_dir(&package.installation_dir)?;
        let canonical_installation_dir = package.installation_dir.canonicalize().ok();
        let mut owned_links = Vec::new();
        for binary in &package.binaries {
            let path = package.bin_dir.join(binary);
            let is_owned = fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
                && path.canonicalize().is_ok_and(|target| {
                    canonical_installation_dir
                        .as_ref()
                        .is_some_and(|installation_dir| {
                            target != *installation_dir && target.starts_with(installation_dir)
                        })
                });
            if is_owned {
                let target = fs::read_link(&path)?;
                owned_links.push((path, target));
            }
        }
        let quarantine = Builder::new()
            .prefix("tmp-uninstall-")
            .tempdir_in(&self.scope.package_root)?;
        let saved = quarantine.path().join("package");
        let mut removed_links = Vec::new();
        let removal = (|| -> Result<()> {
            let transaction = self.database.transaction()?;
            db::remove_package(&transaction, id)?;
            for (path, target) in owned_links {
                fs::remove_file(&path)
                    .with_context(|| format!("remove command link {}", path.display()))?;
                removed_links.push((path, target));
            }
            if package.installation_dir.exists() {
                fs::rename(&package.installation_dir, &saved)?;
            }
            transaction.commit()?;
            Ok(())
        })();
        if let Err(error) = removal {
            if saved.exists() {
                let _ = fs::rename(&saved, &package.installation_dir);
            }
            for (path, target) in removed_links {
                let _ = symlink(target, path);
            }
            return Err(error);
        }
        quarantine
            .close()
            .context("remove quarantined package contents")?;
        println!("Uninstalled {id} in {}", self.scope.description());
        Ok(())
    }
}

struct PendingUpdate {
    installed: PackageRecord,
    resolved: ResolvedPackage,
    version: Option<String>,
    version_validators: Option<HttpValidators>,
}

impl PendingUpdate {
    fn summary(&self) -> String {
        let next = self.version.as_deref().or(self.resolved.tag.as_deref());
        format_update_summary(
            &self.installed.id,
            self.installed.current_version.as_deref(),
            next,
        )
    }
}

fn format_update_summary(id: &PackageId, current: Option<&str>, next: Option<&str>) -> String {
    match (current, next) {
        (Some(current), Some(next)) => format!("Update available {id}: {current} -> {next}"),
        _ => format!("Update available {id}"),
    }
}

enum UpdateProbe {
    Unchanged,
    Skipped(&'static str),
    Available(Box<PendingUpdate>),
}

struct Prepared {
    temp: TempDir,
    root: PathBuf,
    binaries: Vec<PathBuf>,
    asset_url: String,
    validators: HttpValidators,
}

#[derive(Debug)]
struct NoCompatibleExecutable;

impl fmt::Display for NoCompatibleExecutable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("asset has no host-compatible executable")
    }
}

impl Error for NoCompatibleExecutable {}

impl Prepared {
    fn binary_names(&self) -> Vec<String> {
        self.binaries
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    }

    fn links(&self, install_dir: &Path, bin_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
        self.binaries
            .iter()
            .map(|binary| {
                let relative = binary.strip_prefix(&self.root).expect("binary below root");
                (
                    bin_dir.join(binary.file_name().expect("binary has name")),
                    install_dir.join(relative),
                )
            })
            .collect()
    }

    fn apply_rename_rules(&mut self, rules: &[RenameRule]) -> Result<()> {
        for RenameRule(from, to) in rules {
            let Some(index) = self
                .binaries
                .iter()
                .position(|path| path.file_name().is_some_and(|name| name == from.as_str()))
            else {
                bail!("rename source is not a discovered binary: {from}")
            };
            let target = self.binaries[index].with_file_name(to);
            if fs::symlink_metadata(&target).is_ok()
                || self.binaries.iter().any(|binary| binary == &target)
            {
                bail!("rename target already exists: {to}")
            }
            fs::rename(&self.binaries[index], &target)?;
            self.binaries[index] = target;
        }
        self.binaries.sort();
        Ok(())
    }
}

fn prepare(client: &Client, scope: &Scope, package: &ResolvedPackage) -> Result<Prepared> {
    let temp = Builder::new()
        .prefix("tmp-")
        .tempdir_in(&scope.package_root)?;
    let host = compat::Host::current()?;
    let mut failures = Vec::new();
    for (index, candidate) in package.candidates.iter().enumerate() {
        match prepare_candidate(client, &temp, package, candidate, index, host) {
            Ok((root, binaries, validators)) => {
                return Ok(Prepared {
                    temp,
                    root,
                    binaries,
                    asset_url: candidate.url.clone(),
                    validators,
                });
            }
            Err(error) => failures.push((candidate.name.clone(), error)),
        }
    }
    Err(preparation_failure(failures))
}

fn preparation_failure(mut failures: Vec<(String, anyhow::Error)>) -> anyhow::Error {
    if failures.len() == 1 && !failures[0].1.is::<NoCompatibleExecutable>() {
        let (name, error) = failures.pop().expect("one candidate failure");
        return error.context(name);
    }

    let compatibility_only = !failures.is_empty()
        && failures
            .iter()
            .all(|(_, error)| error.is::<NoCompatibleExecutable>());
    let summary = if compatibility_only {
        "no release asset contained a compatible executable"
    } else {
        "failed to prepare any release asset"
    };
    let details = failures
        .iter()
        .map(|(name, error)| format!("{name}: {error:#}"))
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::anyhow!("{summary}\n{details}")
}

fn prepare_candidate(
    client: &Client,
    temp: &TempDir,
    package: &ResolvedPackage,
    candidate: &AssetCandidate,
    index: usize,
    host: compat::Host,
) -> Result<(PathBuf, Vec<PathBuf>, HttpValidators)> {
    let candidate_root = temp.path().join(format!("candidate-{index}"));
    let tree = candidate_root.join("tree");
    fs::create_dir_all(&candidate_root)?;
    let payload = candidate_root.join("payload");
    let mut response = source::asset_response(client, package, candidate)?;
    let validators = validators(response.headers());
    let total = response.content_length();
    let progress = total
        .map(ProgressBar::new)
        .unwrap_or_else(ProgressBar::new_spinner);
    progress.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} {bytes}/{total_bytes} {wide_bar:.cyan/blue}",
        )?
        .progress_chars("=> "),
    );
    let mut output = File::create(&payload)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        progress.inc(count as u64);
    }
    progress.finish_and_clear();
    archive::extract(&payload, &candidate.name, &package.app, &tree)?;
    let root = compat::descend_single_root(tree)?.canonicalize()?;
    let has_modes = matches!(
        archive::format(&candidate.name),
        Format::SevenZ
            | Format::Zip
            | Format::Tar
            | Format::TarGz
            | Format::TarBz2
            | Format::TarXz
            | Format::TarZst
    );
    let mut binaries = compat::executable_candidates(&root, has_modes, host)?;
    if binaries.is_empty() {
        return Err(NoCompatibleExecutable.into());
    }
    if binaries.len() == 1 && platform_suffixed(&binaries[0], host.os) {
        let renamed = binaries[0].with_file_name(&package.app);
        if renamed != binaries[0] {
            fs::rename(&binaries[0], &renamed)?;
            binaries[0] = renamed;
        }
    }
    Ok((root, binaries, validators))
}

fn direct_changed(
    client: &Client,
    installed: &PackageRecord,
    resolved: &ResolvedPackage,
    version: Option<&str>,
) -> Result<bool> {
    if installed.source_kind != SourceKind::Direct {
        return Ok(installed.current_version != resolved.tag);
    }
    if version.is_some() {
        return Ok(installed.current_version.as_deref() != version);
    }
    if installed.validators.etag.is_none() && installed.validators.last_modified.is_none() {
        return Ok(false);
    }
    let response = source::conditional_head(
        client,
        &resolved.candidates[0].url,
        installed.validators.etag.as_deref(),
        installed.validators.last_modified.as_deref(),
    )?;
    Ok(response.status() != StatusCode::NOT_MODIFIED
        && validators(response.headers()) != installed.validators)
}

fn fetch_version(client: &Client, url: &str) -> Result<(String, HttpValidators)> {
    let response = source::direct_response(client, url)?;
    let response_validators = validators(response.headers());
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .context("version response has no Content-Type header")?
        .to_str()
        .context("version response Content-Type is not valid text")?;
    let response_kind = version_response_kind(content_type)?;
    let mut bytes = Vec::new();
    response
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .context("read version response")?;
    if bytes.len() > 64 * 1024 {
        bail!("version response exceeds 65536 bytes")
    }
    let body = String::from_utf8(bytes).context("version response is not UTF-8")?;
    let value = extract_version(&body, response_kind)?;
    if value.is_empty() || value.len() > 64 {
        bail!("resolved version must contain 1 to 64 bytes")
    }
    Ok((value.to_owned(), response_validators))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VersionResponseKind {
    Json,
    PlainText,
}

fn version_response_kind(content_type: &str) -> Result<VersionResponseKind> {
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    let Some((top_level, subtype)) = media_type.split_once('/') else {
        bail!("unsupported or malformed version response Content-Type: {content_type}")
    };
    let subtype = subtype.to_ascii_lowercase();
    if top_level.eq_ignore_ascii_case("application")
        && (subtype == "json" || subtype.ends_with("+json"))
    {
        return Ok(VersionResponseKind::Json);
    }
    if top_level.eq_ignore_ascii_case("text") && subtype == "plain" {
        return Ok(VersionResponseKind::PlainText);
    }
    bail!("unsupported or malformed version response Content-Type: {content_type}")
}

fn extract_version(body: &str, response_kind: VersionResponseKind) -> Result<&str> {
    match response_kind {
        VersionResponseKind::Json => {
            let pattern = Regex::new(r#"(?s)\"(?:version|latest)\"\s*:\s*\"([^\"]+)\""#)?;
            pattern
                .captures(body)
                .and_then(|captures| captures.get(1))
                .map(|value| value.as_str().trim())
                .context("JSON version response does not contain a version or latest string")
        }
        VersionResponseKind::PlainText => body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .context("plain-text version response has no non-empty lines"),
    }
}

fn validators(headers: &reqwest::header::HeaderMap) -> HttpValidators {
    HttpValidators {
        etag: headers
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        last_modified: headers
            .get(LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    }
}

fn forge_source(package: &PackageRecord) -> String {
    let base = package
        .id
        .as_str()
        .strip_suffix(
            package
                .release_selector
                .as_deref()
                .map(|selector| format!(":{selector}"))
                .as_deref()
                .unwrap_or(""),
        )
        .unwrap_or(package.id.as_str());
    format!("https://{base}")
}

fn platform_suffixed(path: &Path, os: compat::HostOs) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let markers: &[&str] = match os {
        compat::HostOs::Linux => &["linux"],
        compat::HostOs::Macos => &["mac", "macos", "darwin"],
    };
    markers.iter().any(|marker| {
        ["-", "_", "."]
            .iter()
            .any(|delimiter| name.contains(&format!("{delimiter}{marker}")))
    })
}

fn format_package(package: &PackageRecord) -> String {
    format!(
        "{}\t{}\t{}\t{}",
        package.id,
        package.current_version.as_deref().unwrap_or("-"),
        if package.pinned { "pinned" } else { "tracking" },
        package.binaries.join(",")
    )
}

fn package_not_found(database: &Database, id: &str) -> String {
    if id.contains('/') {
        return format!("package ID not installed: {id}");
    }
    match database.owners_of_binary(id) {
        Ok(owners) if !owners.is_empty() => format!(
            "package ID not installed: {id}; command is provided by {}",
            owners.join(", ")
        ),
        _ => format!("package ID not installed: {id}"),
    }
}

fn open_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open lock {}", path.display()))
}

fn effective_pin(
    requested: Option<bool>,
    installed: Option<bool>,
    reinstall: bool,
    automatic: bool,
) -> bool {
    match installed {
        None => requested.unwrap_or(automatic),
        Some(installed) if reinstall => requested.unwrap_or(installed),
        Some(installed) => requested.unwrap_or(installed || automatic),
    }
}

fn pin_policy(
    pinned: bool,
    source_kind: SourceKind,
    version_check_url: Option<&str>,
    validators: &HttpValidators,
    requested: Option<bool>,
) -> (bool, Option<PinNotice>) {
    if source_kind != SourceKind::Direct
        || version_check_url.is_some()
        || validators.etag.is_some()
        || validators.last_modified.is_some()
    {
        return (pinned, None);
    }

    let notice = match requested {
        Some(true) => None,
        Some(false) => Some(PinNotice::UnpinIgnored),
        None => Some(PinNotice::Automatic),
    };
    (true, notice)
}

fn acquire_lock(lock: &File) -> Result<()> {
    for attempt in 1..=10 {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                eprintln!("Waiting for another eget process ({attempt}/10)...");
                if attempt < 10 {
                    thread::sleep(Duration::from_secs(1));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    bail!("another eget process still holds the package lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_non_compatibility_failure_is_returned_without_a_misleading_summary() {
        let error = preparation_failure(vec![(
            "tool-linux.zip".into(),
            anyhow::anyhow!("network unavailable"),
        )]);

        assert_eq!(format!("{error:#}"), "tool-linux.zip: network unavailable");
        assert!(!error.to_string().contains("compatible executable"));
    }

    #[test]
    fn compatibility_summary_is_reserved_for_compatibility_failures() {
        let error = preparation_failure(vec![
            ("tool-glibc.zip".into(), NoCompatibleExecutable.into()),
            ("tool-musl.zip".into(), NoCompatibleExecutable.into()),
        ]);

        assert!(
            error
                .to_string()
                .starts_with("no release asset contained a compatible executable")
        );
        assert!(format!("{error:#}").contains("tool-glibc.zip"));
        assert!(format!("{error:#}").contains("tool-musl.zip"));
    }

    #[test]
    fn mixed_candidate_failures_use_a_neutral_summary() {
        let error = preparation_failure(vec![
            ("tool-glibc.zip".into(), NoCompatibleExecutable.into()),
            (
                "tool-musl.zip".into(),
                anyhow::anyhow!("connection reset by peer"),
            ),
        ]);

        assert!(
            error
                .to_string()
                .starts_with("failed to prepare any release asset")
        );
        assert!(format!("{error:#}").contains("tool-glibc.zip"));
        assert!(format!("{error:#}").contains("tool-musl.zip: connection reset by peer"));
    }

    #[test]
    fn list_format_uses_nullable_version() {
        let record = PackageRecord {
            id: PackageId::parse("example.com/tool").unwrap(),
            current_version: None,
            owner: "example.com".into(),
            app: "tool".into(),
            source_kind: SourceKind::Direct,
            installation_dir: "/tmp/pkg".into(),
            bin_dir: "/tmp/bin".into(),
            pinned: false,
            installed_asset_url: "https://example.com/tool".into(),
            channel: None,
            release_selector: None,
            version_check_url: None,
            validators: HttpValidators::default(),
            rename_rules: Vec::new(),
            installed_at: "now".into(),
            updated_at: None,
            binaries: vec!["tool".into()],
        };
        assert_eq!(
            format_package(&record),
            "example.com/tool\t-\ttracking\ttool"
        );
    }

    #[test]
    fn exact_versions_automatically_pin_normal_installs() {
        assert!(effective_pin(None, None, false, true));
        assert!(effective_pin(None, Some(false), false, true));
        assert!(!effective_pin(Some(false), Some(false), false, true));
        assert!(!effective_pin(None, Some(false), false, false));
    }

    #[test]
    fn reinstalls_preserve_stored_pin_policy_unless_explicitly_changed() {
        assert!(!effective_pin(None, Some(false), true, true));
        assert!(effective_pin(None, Some(true), true, false));
        assert!(effective_pin(Some(true), Some(false), true, false));
        assert!(!effective_pin(Some(false), Some(true), true, true));
    }

    #[test]
    fn validatorless_direct_downloads_force_pinning() {
        let validators = HttpValidators::default();
        assert_eq!(
            pin_policy(false, SourceKind::Direct, None, &validators, None),
            (true, Some(PinNotice::Automatic))
        );
        assert_eq!(
            pin_policy(false, SourceKind::Direct, None, &validators, Some(false)),
            (true, Some(PinNotice::UnpinIgnored))
        );
        assert_eq!(
            pin_policy(false, SourceKind::Direct, None, &validators, Some(true)),
            (true, None)
        );
    }

    #[test]
    fn trackable_downloads_preserve_their_pin_policy() {
        let etag = HttpValidators {
            etag: Some("asset-one".into()),
            last_modified: None,
        };
        assert_eq!(
            pin_policy(false, SourceKind::Direct, None, &etag, None),
            (false, None)
        );
        let last_modified = HttpValidators {
            etag: None,
            last_modified: Some("Mon, 20 Jul 2026 00:00:00 GMT".into()),
        };
        assert_eq!(
            pin_policy(false, SourceKind::Direct, None, &last_modified, None),
            (false, None)
        );
        assert_eq!(
            pin_policy(
                false,
                SourceKind::Direct,
                Some("https://example.com/version"),
                &HttpValidators::default(),
                None
            ),
            (false, None)
        );
        assert_eq!(
            pin_policy(
                false,
                SourceKind::Github,
                None,
                &HttpValidators::default(),
                None
            ),
            (false, None)
        );
    }

    #[test]
    fn available_update_summaries_include_versions_when_known() {
        let id = PackageId::parse("github.com/owner/tool").unwrap();
        assert_eq!(
            format_update_summary(&id, Some("v1"), Some("v2")),
            "Update available github.com/owner/tool: v1 -> v2"
        );
        assert_eq!(
            format_update_summary(&id, None, None),
            "Update available github.com/owner/tool"
        );
    }

    #[test]
    fn version_response_content_types_select_the_expected_parser() {
        assert_eq!(
            version_response_kind("application/json").unwrap(),
            VersionResponseKind::Json
        );
        assert_eq!(
            version_response_kind("Application/Problem+JSON; charset=utf-8").unwrap(),
            VersionResponseKind::Json
        );
        assert_eq!(
            version_response_kind("text/plain; charset=UTF-8").unwrap(),
            VersionResponseKind::PlainText
        );
        assert!(version_response_kind("text/html").is_err());
        assert!(version_response_kind("json").is_err());
    }

    #[test]
    fn json_version_responses_require_a_matching_field() {
        assert_eq!(
            extract_version(
                r#"{"name":"tool","latest": " v2 "}"#,
                VersionResponseKind::Json
            )
            .unwrap(),
            "v2"
        );
        assert!(extract_version(r#"{"name":"tool"}"#, VersionResponseKind::Json).is_err());
    }

    #[test]
    fn plain_text_version_responses_use_the_first_non_empty_line() {
        assert_eq!(
            extract_version("\n \t\r\n  v3  \nignored\n", VersionResponseKind::PlainText).unwrap(),
            "v3"
        );
        assert!(extract_version("\n \t\r\n", VersionResponseKind::PlainText).is_err());
    }
}
