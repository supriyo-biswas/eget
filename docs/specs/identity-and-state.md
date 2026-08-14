# Identity and state

Related specifications: [overview](eget.md), [scopes and storage](scopes-and-storage.md),
and [source resolution](source-resolution.md).

## Package/application IDs

Package and application IDs are used throughout the system, defined by the following EBNF grammar:

```
PackageId = PackageSource "/" ApplicationId
PackageSource = PackageAddress Port?
PackageAddress = DNSName # punycoded, last dot removed if it exists
    | IPv4Address # w.x.y.z
    | IPv6Address # the usual format that occurs in URLs, e.g. `[2a00:1abc:3df::e01]`
Port = ':' [0-9]{1,5} # 1-65535

ApplicationId = ApplicationName MonorepoPart?
ApplicationName = ApplicationNamePart ("/" ApplicationNamePart)*
MonorepoPart = ':' Selector
Selector = ApplicationNamePart ("/" ApplicationNamePart)*
ApplicationNamePart = [a-zA-Z0-9_.-]+
```

A package ID therefore consists of a package source followed by one or more application-name components and looks like `<domain>[:<port>]/<application-path>[:<tag>]`, e.g.:

* `github.com/BurntSushi/ripgrep` — a GitHub repo.
* `gitlab.com/my-group/my-subgroup/my-app` — GitLab supports nested subgroups, which the `ApplicationName` grammar already accommodates via repeated `"/" ApplicationNamePart` segments.
* `min.io/mc` — a direct-URL-derived package, where the application name is derived from the URL (see [Application name derivation for direct URLs](source-resolution.md#application-name-derivation-for-direct-urls)).
* `gitlab.acmecorp.com/team/tool:v2` — the trailing `:v2` is the `MonorepoPart`, used as a release/tag selector.

When a package ID is split into the `packages.owner` and `packages.app` metadata fields, `app` is the final `ApplicationNamePart` and `owner` is everything before it, including the package source/domain. A trailing `MonorepoPart` is not part of either field. For example:

| Package ID | `owner` | `app` |
| --- | --- | --- |
| `min.io/mc` | `min.io` | `mc` |
| `github.com/BurntSushi/ripgrep` | `github.com/BurntSushi` | `ripgrep` |
| `github.com/BurntSushi/rg` | `github.com/BurntSushi` | `rg` |
| `gitlab.com/my-group/my-subgroup/my-app:v2` | `gitlab.com/my-group/my-subgroup` | `my-app` |

Thus, `owner` does not mean only the forge's repository-owner path: for a forge package it is the domain plus that path. The `app` value comes from the ID itself; it is not inferred from a discovered binary name (so `github.com/BurntSushi/ripgrep` has `app = 'ripgrep'`, while an ID ending in `/rg` has `app = 'rg'`). This split is structural and unambiguous even for nested GitLab groups and direct-URL-derived IDs.

### Application ID hash

The on-disk installation directory for a package is named after a hash of its full package ID. Package IDs use the `{package-source}/{application-name}[:{selector}]` form throughout. The `packages.owner`/`packages.app` split described above does not change or add components to this canonical ID. The same string used for install-time identity is stored as `packages.id` and accepted by `uninstall`, `mark`, and `update`. Specifically:

```
applicationIdHash = base32(xxh3_128(packageId))
```

i.e., the package ID string is hashed with XXH3 (128-bit variant, taken as its raw bytes — not a hex-string re-encoding of the integer), and those raw hash bytes are Base32-encoded per RFC 4648, **lowercased, with padding (`=`) removed**, to produce a filesystem-safe directory name. The package's contents live at `{$packageFilesDir}/{applicationIdHash}`.

## Metadata DB

Metadata regarding packages is stored in the metadata DB, a SQLite file.

The definitions below are the exact SQLite schema. All tables are `STRICT` and `WITHOUT ROWID`; primary-key columns are explicitly `NOT NULL`. Values represented as enums or booleans are validated when read and written by the application.

### Migration table

Stores migrations, i.e. incremental updates that newer versions of `eget` may apply to the schema.

```sql
CREATE TABLE migrations (
    id INTEGER PRIMARY KEY NOT NULL,
    -- 1 means applied
    state INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
```

Migration IDs start at 1, are contiguous, and have `state = 1`. On every invocation, `eget` verifies that the row count, maximum ID, and states exactly describe the schema understood by the running binary. Future, missing, duplicate, or inactive migration state is rejected before package operations begin.

### Packages table

Stores information about each installed package.

```sql
CREATE TABLE packages (
    -- the package ID
    id TEXT PRIMARY KEY NOT NULL,
    -- null for direct packages installed without --version-url
    current_version TEXT,
    -- parts of the id which will be described later
    -- package ID without its final ApplicationNamePart or optional MonorepoPart;
    -- includes the source domain, e.g. 'github.com/BurntSushi' or 'min.io'
    owner TEXT NOT NULL,
    -- final ApplicationNamePart, excluding any MonorepoPart, e.g. 'ripgrep' or 'mc'
    app TEXT NOT NULL,
    source_kind TEXT NOT NULL, -- ENUM('github', 'gitlab', 'gitea', 'direct')
    -- directory where the package files are installed, i.e. `{$packageFilesDir}/{applicationIdHash}`
    installation_dir TEXT NOT NULL,
    -- location where the discovered binaries' symlinks are installed
    bin_dir TEXT NOT NULL,
    -- pinned packages are not updated when new versions are available
    pinned INTEGER NOT NULL, -- BOOLEAN
    -- URL of the asset that has been downloaded and installed
    installed_asset_url TEXT NOT NULL,
    -- represents any special channels to fetch from, e.g. 'stable'/'prerelease', etc.
    channel TEXT,
    -- the MonorepoPart / release tag selector; also recorded when the derived tag
    -- prefix matches the repository name (e.g. repo jq, tag jq-1.8.2)
    release_selector TEXT,
    -- when source_kind='direct' and --version-url was passed at install time, the URL to
    -- check for the current version string (see Direct-URL version tracking)
    version_check_url TEXT,
    -- http header values used to cheaply detect "no change" on update: for direct packages
    -- installed with --version-url these are the etag/last-modified of version_check_url;
    -- otherwise (no --version-url) they are the etag/last-modified of installed_asset_url itself
    etag TEXT,
    last_modified TEXT,
    -- rules to rename discovered binaries, a JSON list<tuple<string, string>>
    rename_rules TEXT NOT NULL,
    -- when it was installed/last updated
    installed_at TEXT NOT NULL, -- DATETIME, ISO-8601
    updated_at TEXT, -- DATETIME, ISO-8601
    -- JSON list<string> of automatic asset-name preferences captured during installation;
    -- null only for a package migrated from schema version 1 which has not yet been updated
    asset_preferences TEXT
) STRICT, WITHOUT ROWID;
```

`id` is the sole package primary key. There is one row per installed package, and version history is not retained beyond `current_version`.

`asset_preferences` is set automatically and has no command-line or manifest control. New installations store `[]`, `["gtk"]`, or `["qt"]`. A package migrated from schema version 1 retains `NULL` until its next successful installation or update, which detects and records the current preference. Once non-null, the value is preserved across targeted updates and `--reinstall`; uninstalling and freshly installing the package detects it again.

### Binaries table

Tracks the final binary names provided by each package, after rename rules. The command-link path for a row is derived by joining `packages.bin_dir` with `binary_name`.

```sql
CREATE TABLE binaries (
    package_id TEXT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    binary_name TEXT NOT NULL,
    PRIMARY KEY (package_id, binary_name)
) STRICT, WITHOUT ROWID;
```

### Source probe cache

Caches the result of probing a custom domain to determine whether it is a
Gitea or GitLab instance (or neither), so `eget` does not need
to re-probe on every invocation.

```sql
CREATE TABLE source_probe_cache (
    domain TEXT PRIMARY KEY NOT NULL,
    -- ENUM('gitea', 'gitlab', 'unknown') — the probe result for this domain
    kind TEXT NOT NULL,
    checked_at INTEGER NOT NULL -- unix timestamp of when the probe was last performed
) STRICT, WITHOUT ROWID;
```

A cached row expires after **12 hours** (currently a hardcoded constant, not user-configurable). Once expired, the domain is re-probed on next use per the [Probe algorithm](source-resolution.md#domain-probing).

### Schema compatibility and reset

The schema above is recognized by its exact `packages` column set and validated migration history. Migration IDs are contiguous: migration 1 creates the original tables and migration 2 adds `packages.asset_preferences`. An empty database is initialized with all four tables and both migrations in one transaction. A version-1 database is upgraded transactionally without changing existing package metadata; its existing package rows receive `NULL` asset preferences. A database with an unrecognized shape or schema/history mismatch is rejected and is never deleted automatically.

One pre-schema layout is recognized for destructive cleanup: a `packages` table containing `source_url`, `resolved_url`, `release_tag`, and `install_dir`, together with a `links` table. Its tracked package directories and links are not imported. Before deleting that database, `eget` removes only links that are still symlinks resolving beneath their recorded package directory and only package directories whose canonical paths are strict descendants of the active package root. Modified/unowned links are preserved, and an out-of-root package path aborts the reset. The database, WAL, and shared-memory files are removed only after those safety checks and filesystem cleanup succeed.
