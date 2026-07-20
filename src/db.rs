use crate::model::{HttpValidators, PackageId, PackageRecord, ProbeKind, RenameRule, SourceKind};
use crate::policy::Channel;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

const LATEST_MIGRATION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE migrations (
    id INTEGER PRIMARY KEY NOT NULL,
    state INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE packages (
    id TEXT PRIMARY KEY NOT NULL,
    current_version TEXT,
    owner TEXT NOT NULL,
    app TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    installation_dir TEXT NOT NULL,
    bin_dir TEXT NOT NULL,
    pinned INTEGER NOT NULL,
    installed_asset_url TEXT NOT NULL,
    channel TEXT,
    release_selector TEXT,
    version_check_url TEXT,
    etag TEXT,
    last_modified TEXT,
    rename_rules TEXT NOT NULL,
    installed_at TEXT NOT NULL,
    updated_at TEXT
) STRICT, WITHOUT ROWID;

CREATE TABLE binaries (
    package_id TEXT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    binary_name TEXT NOT NULL,
    PRIMARY KEY (package_id, binary_name)
) STRICT, WITHOUT ROWID;

CREATE TABLE source_probe_cache (
    domain TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    checked_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO migrations(id, state) VALUES (1, 1);
"#;

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn reset_legacy_at(path: &Path, package_root: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        match schema_kind(path)? {
            SchemaKind::Legacy => {
                reset_legacy(path, package_root)?;
                Ok(true)
            }
            SchemaKind::Empty => Ok(false),
            SchemaKind::Current | SchemaKind::Unknown => bail!(
                "unexpected database at legacy state path {}; refusing automatic reset",
                path.display()
            ),
        }
    }

    pub fn open(path: &Path, package_root: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        if path.exists() {
            match schema_kind(path)? {
                SchemaKind::Current => {}
                SchemaKind::Legacy => reset_legacy(path, package_root)?,
                SchemaKind::Unknown => {
                    bail!(
                        "unsupported database schema at {}; refusing automatic reset",
                        path.display()
                    )
                }
                SchemaKind::Empty => {}
            }
        }

        let mut connection =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        Ok(self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    pub fn package(&self, id: &str) -> Result<Option<PackageRecord>> {
        let package = self
            .connection
            .query_row(PACKAGE_SELECT_BY_ID, [id], row_package)
            .optional()?;
        package
            .map(|mut package| {
                package.binaries = binaries_for(&self.connection, package.id.as_str())?;
                Ok(package)
            })
            .transpose()
    }

    pub fn packages(&self) -> Result<Vec<PackageRecord>> {
        let mut statement = self.connection.prepare(PACKAGE_SELECT_ALL)?;
        let packages = statement
            .query_map([], row_package)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        packages
            .into_iter()
            .map(|mut package| {
                package.binaries = binaries_for(&self.connection, package.id.as_str())?;
                Ok(package)
            })
            .collect()
    }

    pub fn package_ids(&self) -> Result<Vec<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM packages ORDER BY id")?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn now(&self) -> Result<String> {
        Ok(self
            .connection
            .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |row| {
                row.get(0)
            })?)
    }

    pub fn owners_of_binary(&self, binary: &str) -> Result<Vec<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT package_id FROM binaries WHERE binary_name=?1 ORDER BY package_id")?;
        Ok(statement
            .query_map([binary], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn probe(&self, domain: &str) -> Result<Option<(ProbeKind, i64)>> {
        self.connection
            .query_row(
                "SELECT kind, checked_at FROM source_probe_cache WHERE domain=?1",
                [domain],
                |row| {
                    let kind: String = row.get(0)?;
                    let kind = ProbeKind::from_str(&kind).map_err(sql_conversion)?;
                    Ok((kind, row.get(1)?))
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn put_probe(&self, domain: &str, kind: ProbeKind, checked_at: i64) -> Result<()> {
        self.connection.execute(
            "INSERT INTO source_probe_cache(domain,kind,checked_at) VALUES(?1,?2,?3)
             ON CONFLICT(domain) DO UPDATE SET kind=excluded.kind, checked_at=excluded.checked_at",
            params![domain, kind.as_str(), checked_at],
        )?;
        Ok(())
    }

    pub fn remove_probe(&self, domain: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM source_probe_cache WHERE domain=?1", [domain])?;
        Ok(())
    }
}

pub fn replace_package(transaction: &Transaction<'_>, package: &PackageRecord) -> Result<()> {
    transaction.execute("DELETE FROM packages WHERE id=?1", [package.id.as_str()])?;
    transaction.execute(
        "INSERT INTO packages(
            id,current_version,owner,app,source_kind,installation_dir,bin_dir,pinned,
            installed_asset_url,channel,release_selector,version_check_url,etag,last_modified,
            rename_rules,installed_at,updated_at
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        params![
            package.id.as_str(),
            package.current_version,
            package.owner,
            package.app,
            package.source_kind.as_str(),
            package.installation_dir.to_string_lossy(),
            package.bin_dir.to_string_lossy(),
            package.pinned,
            package.installed_asset_url,
            package.channel.map(Channel::as_str),
            package.release_selector,
            package.version_check_url,
            package.validators.etag,
            package.validators.last_modified,
            serde_json::to_string(&package.rename_rules)?,
            package.installed_at,
            package.updated_at,
        ],
    )?;
    for binary in &package.binaries {
        transaction.execute(
            "INSERT INTO binaries(package_id,binary_name) VALUES(?1,?2)",
            params![package.id.as_str(), binary],
        )?;
    }
    Ok(())
}

pub fn remove_package(transaction: &Transaction<'_>, id: &str) -> Result<()> {
    if transaction.execute("DELETE FROM packages WHERE id=?1", [id])? != 1 {
        bail!("package ID not installed: {id}")
    }
    Ok(())
}

pub fn mark_package(
    transaction: &Transaction<'_>,
    id: &str,
    pin: Option<bool>,
    channel: Option<Channel>,
) -> Result<()> {
    let current: (bool, Option<String>) = transaction
        .query_row(
            "SELECT pinned,channel FROM packages WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .with_context(|| format!("package ID not installed: {id}"))?;
    transaction.execute(
        "UPDATE packages SET pinned=?2,channel=?3 WHERE id=?1",
        params![
            id,
            pin.unwrap_or(current.0),
            channel.map(Channel::as_str).or(current.1.as_deref()),
        ],
    )?;
    Ok(())
}

const PACKAGE_COLUMNS: &str = "id,current_version,owner,app,source_kind,installation_dir,bin_dir,pinned,installed_asset_url,channel,release_selector,version_check_url,etag,last_modified,rename_rules,installed_at,updated_at";
const PACKAGE_SELECT_BY_ID: &str = "SELECT id,current_version,owner,app,source_kind,installation_dir,bin_dir,pinned,installed_asset_url,channel,release_selector,version_check_url,etag,last_modified,rename_rules,installed_at,updated_at FROM packages WHERE id=?1";
const PACKAGE_SELECT_ALL: &str = "SELECT id,current_version,owner,app,source_kind,installation_dir,bin_dir,pinned,installed_asset_url,channel,release_selector,version_check_url,etag,last_modified,rename_rules,installed_at,updated_at FROM packages ORDER BY id";

fn row_package(row: &rusqlite::Row<'_>) -> rusqlite::Result<PackageRecord> {
    let id: String = row.get(0)?;
    let source_kind: String = row.get(4)?;
    let channel: Option<String> = row.get(9)?;
    let rename_rules: String = row.get(14)?;
    Ok(PackageRecord {
        id: PackageId::parse(id).map_err(sql_conversion)?,
        current_version: row.get(1)?,
        owner: row.get(2)?,
        app: row.get(3)?,
        source_kind: SourceKind::from_str(&source_kind).map_err(sql_conversion)?,
        installation_dir: PathBuf::from(row.get::<_, String>(5)?),
        bin_dir: PathBuf::from(row.get::<_, String>(6)?),
        pinned: row.get(7)?,
        installed_asset_url: row.get(8)?,
        channel: channel
            .map(|value| value.parse().map_err(sql_conversion))
            .transpose()?,
        release_selector: row.get(10)?,
        version_check_url: row.get(11)?,
        validators: HttpValidators {
            etag: row.get(12)?,
            last_modified: row.get(13)?,
        },
        rename_rules: serde_json::from_str::<Vec<RenameRule>>(&rename_rules)
            .map_err(sql_conversion)?,
        installed_at: row.get(15)?,
        updated_at: row.get(16)?,
        binaries: Vec::new(),
    })
}

fn binaries_for(connection: &Connection, package_id: &str) -> Result<Vec<String>> {
    let mut statement = connection
        .prepare("SELECT binary_name FROM binaries WHERE package_id=?1 ORDER BY binary_name")?;
    Ok(statement
        .query_map([package_id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn sql_conversion(error: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()).into(),
    )
}

fn migrate(connection: &mut Connection) -> Result<()> {
    if !table_exists(connection, "migrations")? {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(SCHEMA)?;
        transaction.commit()?;
        return Ok(());
    }
    let maximum: Option<i64> =
        connection.query_row("SELECT MAX(id) FROM migrations", [], |row| row.get(0))?;
    if maximum.is_some_and(|id| id > LATEST_MIGRATION) {
        bail!("database migration is newer than this eget supports")
    }
    let invalid_rows: i64 = connection.query_row(
        "SELECT COUNT(*) FROM migrations WHERE id != ?1 OR state != 1",
        [LATEST_MIGRATION],
        |row| row.get(0),
    )?;
    let row_count: i64 =
        connection.query_row("SELECT COUNT(*) FROM migrations", [], |row| row.get(0))?;
    if maximum != Some(LATEST_MIGRATION) || row_count != 1 || invalid_rows != 0 {
        bail!("database has incomplete migration history")
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SchemaKind {
    Empty,
    Current,
    Legacy,
    Unknown,
}

fn schema_kind(path: &Path) -> Result<SchemaKind> {
    let connection = Connection::open(path)?;
    if !table_exists(&connection, "packages")? {
        return Ok(if table_exists(&connection, "migrations")? {
            SchemaKind::Unknown
        } else {
            SchemaKind::Empty
        });
    }
    let package_columns = table_columns(&connection, "packages")?;
    if package_columns == PACKAGE_COLUMNS.split(',').collect::<Vec<_>>() {
        return Ok(SchemaKind::Current);
    }
    let legacy_columns = [
        "id",
        "owner",
        "app",
        "source_kind",
        "source_url",
        "resolved_url",
        "release_tag",
        "install_dir",
        "pinned",
        "channel",
        "release_selector",
        "etag",
        "last_modified",
        "installed_at",
        "updated_at",
    ];
    if legacy_columns
        .iter()
        .all(|column| package_columns.iter().any(|found| found == column))
        && table_exists(&connection, "links")?
    {
        Ok(SchemaKind::Legacy)
    } else {
        Ok(SchemaKind::Unknown)
    }
}

fn reset_legacy(path: &Path, package_root: &Path) -> Result<()> {
    fs::create_dir_all(package_root)?;
    let canonical_root = package_root.canonicalize()?;
    let connection = Connection::open(path)?;
    let mut statement = connection.prepare(
        "SELECT p.install_dir,l.link_path
         FROM packages p LEFT JOIN links l ON l.package_id=p.id
         ORDER BY p.install_dir,l.link_path",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                row.get::<_, Option<String>>(1)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    drop(connection);

    for (install_dir, link_path) in &rows {
        let Some(link_path) = link_path else { continue };
        let link_path = Path::new(link_path);
        if fs::symlink_metadata(link_path).is_ok_and(|metadata| metadata.file_type().is_symlink())
            && link_path
                .canonicalize()
                .is_ok_and(|target| target.starts_with(install_dir))
        {
            fs::remove_file(link_path)
                .with_context(|| format!("remove legacy link {}", link_path.display()))?;
        }
    }
    rows.iter()
        .map(|(install_dir, _)| install_dir)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .try_for_each(|install_dir| -> Result<()> {
            if !install_dir.exists() {
                return Ok(());
            }
            let canonical = install_dir.canonicalize()?;
            if canonical == canonical_root || !canonical.starts_with(&canonical_root) {
                bail!(
                    "legacy installation path is outside package root: {}",
                    install_dir.display()
                )
            }
            fs::remove_dir_all(install_dir)
                .with_context(|| format!("remove legacy package {}", install_dir.display()))
        })?;

    for database_file in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if database_file.exists() {
            fs::remove_file(&database_file)
                .with_context(|| format!("remove legacy database {}", database_file.display()))?;
        }
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    Ok(statement
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn sample(root: &Path) -> PackageRecord {
        PackageRecord {
            id: PackageId::parse("example.com/tool").unwrap(),
            current_version: None,
            owner: "example.com".into(),
            app: "tool".into(),
            source_kind: SourceKind::Direct,
            installation_dir: root.join("packages/tool"),
            bin_dir: root.join("bin"),
            pinned: false,
            installed_asset_url: "https://example.com/tool".into(),
            channel: None,
            release_selector: None,
            version_check_url: None,
            validators: HttpValidators::default(),
            rename_rules: Vec::new(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            updated_at: None,
            binaries: vec!["tool".into()],
        }
    }

    #[test]
    fn creates_exact_schema_and_round_trips_nullable_version() {
        let temp = tempfile::tempdir().unwrap();
        let mut database = Database::open(&temp.path().join("eget.sqlite3"), temp.path()).unwrap();
        let package = sample(temp.path());
        let transaction = database.transaction().unwrap();
        replace_package(&transaction, &package).unwrap();
        transaction.commit().unwrap();
        assert_eq!(
            database.package(package.id.as_str()).unwrap(),
            Some(package)
        );

        for table in ["migrations", "packages", "binaries", "source_probe_cache"] {
            let sql: String = database
                .connection
                .query_row(
                    "SELECT sql FROM sqlite_schema WHERE name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(sql.contains("STRICT"), "{table}");
            assert!(sql.contains("WITHOUT ROWID"), "{table}");
        }
    }

    #[test]
    fn probe_cache_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let database = Database::open(&temp.path().join("eget.sqlite3"), temp.path()).unwrap();
        database
            .put_probe("forge.example", ProbeKind::Gitea, 42)
            .unwrap();
        assert_eq!(
            database.probe("forge.example").unwrap(),
            Some((ProbeKind::Gitea, 42))
        );
    }

    #[test]
    fn recognized_legacy_state_is_reset_without_removing_unowned_links() {
        let temp = tempfile::tempdir().unwrap();
        let package_root = temp.path().join("packages");
        let install_dir = package_root.join("legacy");
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(install_dir.join("tool"), "legacy").unwrap();
        let owned = bin_dir.join("tool");
        symlink(install_dir.join("tool"), &owned).unwrap();
        let unowned = bin_dir.join("other");
        fs::write(&unowned, "keep").unwrap();

        let path = temp.path().join("legacy.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE packages (
               id TEXT PRIMARY KEY, owner TEXT, app TEXT, source_kind TEXT,
               source_url TEXT, resolved_url TEXT, release_tag TEXT, install_dir TEXT,
               pinned INTEGER, channel TEXT, release_selector TEXT, etag TEXT,
               last_modified TEXT, installed_at TEXT, updated_at TEXT
             );
             CREATE TABLE links (package_id TEXT, command TEXT, link_path TEXT);",
            )
            .unwrap();
        connection.execute(
            "INSERT INTO packages VALUES(?1,'owner','tool','url','source','asset',NULL,?2,0,'stable',NULL,NULL,NULL,'old','old')",
            params!["legacy/tool", install_dir.to_string_lossy()],
        ).unwrap();
        connection
            .execute(
                "INSERT INTO links VALUES(?1,'tool',?2)",
                params!["legacy/tool", owned.to_string_lossy()],
            )
            .unwrap();
        drop(connection);

        let database = Database::open(&path, &package_root).unwrap();
        assert!(database.packages().unwrap().is_empty());
        assert!(!owned.exists());
        assert!(!install_dir.exists());
        assert_eq!(fs::read_to_string(unowned).unwrap(), "keep");
    }

    #[test]
    fn unknown_and_future_schemas_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let unknown = temp.path().join("unknown.sqlite3");
        Connection::open(&unknown)
            .unwrap()
            .execute_batch("CREATE TABLE packages(id TEXT);")
            .unwrap();
        assert!(Database::open(&unknown, temp.path()).is_err());

        let future = temp.path().join("future.sqlite3");
        let database = Database::open(&future, temp.path()).unwrap();
        database
            .connection
            .execute("INSERT INTO migrations VALUES(2,1)", [])
            .unwrap();
        drop(database);
        assert!(Database::open(&future, temp.path()).is_err());
    }
}
