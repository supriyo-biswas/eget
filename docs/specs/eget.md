This is the specification for the `eget` CLI, a package manager that installs binaries directly from GitHub, Gitea or GitLab projects, or from direct URLs, and manages them thereafter.

## Overview

`eget` has five operations, implemented as subcommands:

* `install` - Install one or more packages
* `list` - List installed packages
* `update` - Update tracked packages
* `uninstall` - Uninstall packages
* `mark` - Change user-provided metadata of a package, such as pinning policy and preferred release channel.

At a high level, installation involves:
1. Resolving whatever the user provided (a URL, package ID, or application ID) into a concrete package ID and a downloadable artifact.
2. Downloading that artifact from the resolved source.
3. Unpacking it into a package files directory.
4. Finding binaries in the package and symlinking them into a desired location.
5. Storing metadata required to manage the installation (for later `list`, `update`, `uninstall`, `mark`).

All commands require a package ID except for `install`, which can work with a URL (e.g. `http://example.com/foo/bar/baz.tgz`), a package ID, or an application ID. These terms are defined in [Package/application IDs](#packageapplication-ids).

The `install` subcommand may be omitted, so `eget my/pkg` means `eget install my/pkg`. The command aliases are `install`/`inst`/`i`, `list`/`ls`, and `uninstall`/`remove`/`rm`. After any leading global `--scope` option, argument normalization inserts the implicit `install` command only when the command-position argument contains `/`, as every valid install locref does. Consequently, install-specific options follow the locref in the implicit form (for example, `eget my/pkg --force`); use the explicit form (`eget install --force my/pkg`) to place them first. Slashless values are left for command parsing, so a typo such as `eget in` fails immediately as an unknown subcommand rather than being treated as a URL to install.

Commands that accept multiple packages process every operand even if an earlier operand fails. They report each failure and return a non-zero status if any operand failed.

Running any package management command acquires a lock so that no more than one instance of `eget` may mutate state at a given time (see [Locking](#locking)).

## Storage

`eget` stores four things as part of its normal operations:

1. **Package metadata**: Information about installed packages, in a SQLite database.
2. **Lock file**: To prevent concurrent access.
3. **Package contents**: The full contents of the package downloaded from one of the sources.
4. **Binary symlinks**: Symlinks pointing at binaries found inside a package.

These are stored in the following locations, where `$var` represents an internal variable name:

* **Metadata DB:** `{$stateDir}/eget/eget.sqlite3`
* **Lock file:** `{$lockDir}/eget.lock`
* **Package files:** `{$packageFilesDir}`
* **Symlinks:** `{$binDir}`

The default values of these variables depend on the scope, determined by the environment variable `EGET_SCOPE` or the global flag `--scope`, which is one of `system`, `user`, or `local`.

For root users, the default scope is `system`; they may choose between `system`, `user`, or `local`. For non-root users, the default scope is `user`; they may choose between `user` or `local` — `system` is disallowed.

The following directories are used, where `$ENV` is an environment variable of that name. For the XDG environment variables, the appropriate fallback values documented in the FreeDesktop XDG specification are used when unset. Where multiple variable names are listed consecutively, they are tried in that order.

|              | `$stateDir`            | `$lockDir`                              | `$packageFilesDir`   | `$binDir`             |
| ------------ | ----------------------- | ---------------------------------------- | --------------------- | --------------------- |
| System scope | `/var/lib/eget`         | `/run/lock`                               | `/opt/eget`           | `/usr/local/bin`      |
| User scope   | `$XDG_DATA_HOME`        | `$XDG_RUNTIME_DIR`<br>`$XDG_DATA_HOME`   | `$XDG_DATA_HOME/eget` | `$HOME/.local/bin`    |
| Local scope  | `$EGET_LOCAL_DATA_DIR`  | `$EGET_LOCAL_LOCK_DIR`                    | `$EGET_LOCAL_PKG_DIR` | `$EGET_LOCAL_BIN_DIR` |

When using the local scope, all of the local-scoped environment variables are mandatory. If the resolved directories do not already exist, `eget` attempts to create them when needed.

The symlink directory may be overridden per-invocation of `install` when in `user` or `system` scope, via the `--to` flag, or the `EGET_BIN_DIR`/`EGET_BIN` environment variables. If more than one is set, precedence is `--to` > `EGET_BIN_DIR` > `EGET_BIN` > the scope's default `$binDir`. These overrides are disallowed in `local` scope.

The selected `bin_dir` is stored per package. Later `update` operations retain it. A repeated `install` without a destination override also retains it; if an override is present and that invocation performs an installation, the package is relinked into the selected directory.

At user scope, the default `$stateDir`/`$packageFilesDir` resolve under `$XDG_DATA_HOME` (per the table above), so a package's on-disk contents live at `$XDG_DATA_HOME/eget/<applicationIdHash>` (commonly `~/.local/share/eget/<applicationIdHash>`, since `$XDG_DATA_HOME` defaults to `~/.local/share`). `<applicationIdHash>` is defined in [Package/application IDs](#packageapplication-ids).

## Locking

Every package management command (`install`, `list`, `update`, `uninstall`, `mark`) acquires an exclusive lock on `{$lockDir}/eget.lock` for its entire duration before touching the metadata DB or the filesystem, and releases it on exit (including on error). This serializes all mutating operations across concurrently-invoked `eget` processes.

If the lock is already held, `eget` does not fail immediately. It prints a user-visible attempt counter and makes 10 attempts, waiting one second between attempts (at most nine seconds of deliberate waiting). If the lock still cannot be acquired on the 10th attempt, `eget` exits with an error.

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
* `min.io/mc` — a direct-URL-derived package, where the application name is derived from the URL (see [Application name derivation for direct URLs](#application-name-derivation-for-direct-urls)).
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
    updated_at TEXT -- DATETIME, ISO-8601
) STRICT, WITHOUT ROWID;
```

`id` is the sole package primary key. There is one row per installed package, and version history is not retained beyond `current_version`.

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

A cached row expires after **12 hours** (currently a hardcoded constant, not user-configurable). Once expired, the domain is re-probed on next use per the [Probe algorithm](#domain-probing).

### Schema compatibility and reset

The schema above is recognized by its exact `packages` column set and validated migration history. An empty database is initialized with all four tables and migration 1 in one transaction. A database with an unrecognized shape is rejected and is never deleted automatically.

One pre-schema layout is recognized for destructive cleanup: a `packages` table containing `source_url`, `resolved_url`, `release_tag`, and `install_dir`, together with a `links` table. Its tracked package directories and links are not imported. Before deleting that database, `eget` removes only links that are still symlinks resolving beneath their recorded package directory and only package directories whose canonical paths are strict descendants of the active package root. Modified/unowned links are preserved, and an out-of-root package path aborts the reset. The database, WAL, and shared-memory files are removed only after those safety checks and filesystem cleanup succeed.

## Package management

### Authentication

For requests made against a forge's API (release listing, asset download), `eget` looks for a bearer/API token in the environment so users can avoid rate limits on unauthenticated requests and access private repos:

* **github.com:** `EGET_GITHUB_TOKEN`, falling back to the plain `GITHUB_TOKEN` environment variable if the former isn't set (mirroring the common convention used by other GitHub-aware CLIs, e.g. the `gh` CLI), sent as `Authorization: Bearer <token>`.
* **gitlab.com:** `EGET_GITLAB_TOKEN`, sent as `PRIVATE-TOKEN: <token>` (GitLab's conventional personal-access-token header).
* **gitea.com:** `EGET_GITEA_TOKEN`, sent as `Authorization: token <token>` (Gitea's conventional header).
* **Any other domain** (a custom GitLab/Gitea instance detected via probing): the token env var name is derived from the domain itself, and the header convention for the detected forge kind (`PRIVATE-TOKEN` for GitLab or `Authorization: token` for Gitea) is used.

**Direct URLs:** for `direct`-kind packages (and `--version-url` checks, see [Direct-URL version tracking](#direct-url-version-tracking)), the same domain-derived `EGET_<DOMAINPART>_TOKEN` scheme is used, but there is no forge-specific convention to apply the value against — the environment variable is expected to already contain the *entire* header value as it should be sent, e.g. `EGET_MIN_IO_TOKEN=Bearer 12345`, and `eget` sends it verbatim as the `Authorization` header. Embedding credentials directly in a locref/URL passed to `eget install` (e.g. `https://user:token@host/...`) is not permitted; `eget` rejects such URLs and requires the token to be supplied via the environment instead.

**Deriving the token env var name (`DOMAINPART`) for a custom domain:**
1. Punycode the domain.
2. Remove a trailing `.com`, if present.
3. Replace every `.` and `-` with `_`.
4. Uppercase the result.
5. The env var is `EGET_<DOMAINPART>_TOKEN`.

Examples:
* `gitlab.acmecorp.com` → `gitlab.acmecorp` → `gitlab_acmecorp` → `EGET_GITLAB_ACMECORP_TOKEN`
* `gitlab.acmecorp.in` → (no `.com` to strip) `gitlab.acmecorp.in` → `gitlab_acmecorp_in` → `EGET_GITLAB_ACMECORP_IN_TOKEN`

This derivation is purely domain-based; forge probing separately determines whether the value uses GitLab or Gitea authentication. Direct URLs use the same variable name with the complete `Authorization` value supplied by the user.

Tokens are attached only while a request remains on the credential origin: the same scheme, hostname, and effective port. Redirects are followed at most 10 times, must remain HTTP(S), and do not receive credentials after crossing to a different origin.

### Domain probing

When a locref's domain is not `github.com`, `gitlab.com`, or `gitea.com`, `eget` must determine which forge (if any) is running at that domain before it can know how to query for releases. This uses the `source_probe_cache` table to avoid repeated network round-trips.

**Probe algorithm**, given a domain `D`:

1. Look up `D` in `source_probe_cache`. If a row exists and `checked_at` is within the last 12 hours, use its cached `kind` and skip network probing.
2. Otherwise, send `GET {origin}/api/v1/version`, preserving the locref's scheme, host, and explicit port.
3. If the response contains any header whose lowercase name starts with `x-gitlab-meta`, classify the origin as **GitLab**. The header is authoritative even on a non-success response.
4. Otherwise, if the response is successful and its body is a JSON object containing a string-valued `version` field, classify the origin as **Gitea**.
5. Otherwise, classify the origin as `unknown`; the locref is treated as a **direct URL**, no release API is consulted, and the URL is downloaded as-is.
6. Upsert `(domain, kind, checked_at=now())` into `source_probe_cache` regardless of outcome, so a domain confirmed not to be a forge is also cached.

A network failure, non-success response without a GitLab marker, oversized response, or malformed JSON does not fail installation by itself; it produces the `unknown`/direct result. No second probe endpoint is attempted.

### Package ID probe (resolving a locref)

A locref ("location reference") is what the user actually types on the command line to `install` — a bare `owner/repo[:tag]`, a full URL to a repo/release page, or a direct download URL. It must be resolved to a concrete package ID (and, for forge-hosted packages, a specific downloadable asset URL) before installation can proceed. A locref always contains at least one `/`.

**Resolution algorithm:**

1. **Bare-shorthand attempt first:** check whether the locref, taken as-is, matches `^\w[-\w]*/\w[-.\w]+(:\w[-\w.]*)?$` — i.e. looks like a plain `owner/repo[:tag]` with no scheme and no further path segments. If it matches, resolve it directly against `github.com` as `github.com/owner/repo[:tag]` and skip the remaining steps entirely.
   * This has to come first, rather than unconditionally prepending `https://` to every locref and then parsing it as a URL: a bare `owner/repo` is not a valid host by itself, and even if `https://` is blindly prepended first, the result (`https://owner/repo`) parses as "host=`owner`, path=`/repo`" — silently producing a nonsense package ID instead of the intended GitHub shorthand. Only if this pattern does *not* match does `eget` fall through to full URL parsing below. (Someone who genuinely means a bare host named `owner` with a path of `repo` needs to spell out the scheme themselves, e.g. `http://owner/repo`, which — having a `://` — never reaches this shorthand check in the first place.)
2. Otherwise, if the locref does not contain `://`, prepend `https://` to it.
3. Parse the locref as a URL.
4. If the URL's scheme is not `http` or `https`, reject it.
5. Normalize the URL: lowercase and strip a trailing `.` from the hostname; punycode the hostname; collapse repeated `/` in the path.
6. **Known-forge shortcut:** if the hostname is exactly `github.com`, `gitlab.com`, or `gitea.com`, the corresponding forge kind is used directly — no probing needed.
7. Otherwise (custom domain), consult [Domain probing](#domain-probing) to determine the forge kind, or fall back to `direct` if the domain is not a recognized forge.
8. **If `source_kind` is `github`/`gitlab`/`gitea`:** split the URL path into segments.
   * The first *N* segments (1 for GitHub/Gitea, 1+ for GitLab where subgroups are allowed) form the repository-owner path; the following and final repository segment is `app`.
   * If the remaining path continues with `releases/tag/<tag>`, that URL-decoded tag is classified according to [Forge suffix and tag classification](#forge-suffix-and-tag-classification).
   * If the remaining path continues with `releases/download/<tag>/<asset>`, the specific asset URL is already fully resolved. The tag is classified for package identity, no release-listing API call is made, and the given URL is downloaded directly.
   * If there is no further path (just `owner/app`), no release selector is recorded and the *latest* release (subject to `channel`, and to [monorepo detection](#monorepo-detection-forge-hosted-packages)) is used.
   * The resulting package ID is `{hostname}/{repository-owner-path}/{app}[:{tag}]`. In stored metadata, `owner` is `{hostname}/{repository-owner-path}`, including the hostname as described above.
9. **If `source_kind` is `direct`:** no repo/release semantics apply. A normalized domain and app name are derived from the URL itself (see below), and the resolved "asset" is the URL as given. The package ID is `{normalized-domain}/{app}`; consequently, the stored `owner` is just `{normalized-domain}` rather than a duplicated `{hostname}/{owner}` value.

### Application name derivation for direct URLs

For `direct`-kind packages (see step 9 above), there is no repository to ask "what's your name" — the normalized domain and app name must be derived heuristically from the URL:

**Normalized domain (and stored `owner`)** = the URL's hostname, with a common CDN/hosting subdomain label stripped from the front *only if* the hostname has at least two dots (i.e., is not already a bare two-label domain). The recognized labels are `www`, `download`, `downloads`, `dl`, `cache`, `cdn`, `release`, `releases`, `assets`, `static`, and `ftp`, each optionally followed by ASCII digits. An explicit non-default port is appended after normalization. For example `dl.min.io` → `min.io`, but `gitlab-docker-machine-downloads.s3.amazonaws.com` is *not* stripped because its first label is not an exact recognized label with an optional numeric suffix.

**App name** = derived from the last path segment:
1. If the URL has no path at all (or only `/`) — i.e. the domain itself serves the binary directly at its root, e.g. `eget https://test.example.com` — the app name is simply `default`, and there's no further stripping to do. This gives a package ID of `test.example.com/default`.
2. Otherwise, take the final `/`-delimited path segment.
3. Lowercase the segment and strip the longest recognized archive suffix: `.7z`, `.zip`, `.tar`, `.tar.gz`, `.tgz`, `.tar.bz2`, `.tbz`, `.tbz2`, `.tar.xz`, `.txz`, `.tar.zst`, `.tzst`, `.gz`, `.bz2`, `.xz`, or `.zst`.
4. Retain the leading run consisting only of ASCII letters, digits, `-`, `_`, and `.`, then trim those delimiters from both ends. If the result does not begin with an ASCII letter, use `default`.
5. At the first delimiter followed by a removable artifact suffix, discard that delimiter and everything after it. A suffix is removable when it begins with a digit, begins with `v` followed by a digit, or begins with a recognized platform/packaging marker: `linux`, `win`, `windows`, `mac`, `macos`, `darwin`, `amd64`, `x86_64`, `x64`, `linux64`, `mac64`, `macos64`, `darwin64`, `arm64`, `aarch64`, `musl`, `glibc`, `gnu`, `static`, or `exe`. A marker may be followed by another `-`, `_`, or `.` component.
6. Trim trailing delimiters again. If no characters remain, use `default`.

Examples:
* `.../v0.16.2-gitlab.51/docker-machine-Linux-x86_64` → last segment `docker-machine-Linux-x86_64` → strip no extension → strip trailing `-Linux-x86_64` → `docker-machine`.
* `https://dl.min.io/aistor/mc/release/linux-amd64/mc` → last segment `mc` → nothing to strip → `mc` (owner: `min.io`, since `dl` is stripped).
* `https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip` → last segment `awscli-exe-linux-x86_64.zip` → strip `.zip` → the `linux` marker begins the removable suffix → `awscli-exe`.

### Forge suffix and tag classification

A suffix after a forge repository (`owner/repo:<suffix>`) or a tag obtained from a release URL is classified before release lookup:

1. A version-led suffix is an exact tag with no release selector. A value is version-led when it begins with an ASCII digit; begins with `v` followed by a digit; begins with `v` plus a version delimiter and contains a digit; or begins with `release`, `version`, `rel`, or `ver` followed by a digit or version delimiter. For example, `v1.2.3` and `2026-07-20` are exact tags.
2. Otherwise, if the suffix contains `-`, `_`, or `/` followed by a version-led remainder, the portion before that delimiter is the selector and the full suffix is an exact tag. For example, `gnu-sed-4.10` is exact tag `gnu-sed-4.10` with selector `gnu-sed`, and `kustomize/v5.8.1` is exact tag `kustomize/v5.8.1` with selector `kustomize`.
3. Otherwise, a valid selector by itself denotes a tracking selector. Selector components begin with an ASCII letter and contain only ASCII letters, digits, `.`, `-`, or `_`; `/` separates components. For example, `gnu-sed` tracks the newest matching tag.
4. Any remaining suffix is an exact tag without a selector.

Exact-tag requests are automatically pinned unless `--unpin`/`--no-pin` is supplied. A selector is appended to the package ID; an exact version without a selector is not. Consequently, installing tracking `owner/repo` and later installing exact tag `owner/repo:v1.2.3` addresses the same base package and leaves it pinned at the exact tag. For repository-named tags such as `jq-1.8.2`, both tracking and exact forms use `github.com/jqlang/jq:jq`; selecting the exact tag changes the existing record from tracking to pinned instead of creating a second package.

### Monorepo detection (forge-hosted packages)

Some forge repos publish releases for more than one distinct artifact family from the same repo — e.g. a repo named `static-builds` whose releases are tagged `gnu-sed-4.1`, `curl-8.2`, `wget-1.5`, etc., ordered by recency, with no single "latest" that makes sense across the whole repo. `eget` needs to detect this situation and require the user to disambiguate, rather than silently installing whatever happens to be the most recent tag regardless of family.

**Detection, when no release selector (`MonorepoPart`) was given** (i.e. plain `owner/repo`):

1. Select the forge's latest release for the requested channel using the provider-specific rules under [Installation](#installation) (for example, GitHub's `/releases/latest` for `stable`).
2. Derive an optional tag prefix using the same rules used for an explicit monorepo selector. For example, repo `jqlang/jq` with latest tag `jq-1.8.2` has prefix `jq`; repo `kustomize-sigs/kustomize` with latest tag `kustomize/v4.5.0` has prefix `kustomize`; version-led tag `v1.2.3` has no prefix.
3. If there is no derived prefix, proceed with the base package ID and leave `packages.release_selector` null.
4. If the derived prefix case-sensitively equals the repo name (the `app` segment), proceed with this release. Retain the prefix as the `MonorepoPart`: the package ID includes it (for example, `github.com/jqlang/jq:jq`) and `packages.release_selector` stores it for later updates. The matching prefix means the unqualified request is unambiguous; it does not mean the selector should be discarded.
5. If a derived prefix does **not** match (e.g. repo `static-builds` with latest tag `gnu-sed-4.1`), `eget` refuses to guess and fails with an error telling the user to re-run with an explicit monorepo selector, e.g. `eget install supriyo-biswas/static-builds:gnu-sed`.

**Once a release selector (`MonorepoPart`) is known** (whether given explicitly, derived from a matching repository-named tag prefix, or loaded from `release_selector` on a prior install), `eget` needs to find the newest release whose tag matches `<selector><boundary-or-end>`. The exact lookup mechanism varies by forge, since none of them expose a "give me the latest *release* whose tag has this prefix" endpoint directly:

* **GitLab**: list releases newest-first and scan them client-side for the first tag satisfying the selector rule.
* **GitHub**: neither the releases nor tags listing endpoint accepts a name/prefix filter. List releases newest-first and scan them client-side for the first tag satisfying the selector rule.
* **Gitea**: its releases and tags REST endpoints likewise have no documented name/prefix filter. List releases newest-first and scan them client-side for the first tag satisfying the selector rule. When the requested channel is `prerelease`, also pass Gitea's `pre-release=true` release-list filter so non-prereleases do not consume the client-side scan.

Every paginated release or tag scan requests the forge's maximum supported page size and fetches at most **5 pages**. If no match is found within those 5 pages, installation/update fails rather than silently choosing a tag from another artifact family or scanning an unbounded history.

This same channel-aware lookup (fetch-latest-and-compare, or forge-appropriate prefix scan once a selector is known) is used both for the initial `install` and for subsequent `update` runs.

### Installation

Given a resolved package ID, source kind, and downloadable asset URL (from the probe/resolution steps above), plus channel/pin selection where applicable:

1. **Release selection** (forge-hosted packages only): select a release for the requested `--channel`, then apply the [monorepo](#monorepo-detection-forge-hosted-packages) selector lookup if a tag prefix was given explicitly (or stored from a prior install). Without a selector, apply the monorepo equality/prefix check to the selected release; if it indicates a monorepo, fail and ask for an explicit selector rather than guessing.
   * `stable` means the newest non-draft release that the forge considers non-prerelease. GitHub and Gitea's latest-release endpoints provide this directly. GitLab has no prerelease flag or prerelease concept in its Releases API, so its newest release by `released_at` is treated as stable.
   * `prerelease` means the newest non-draft release explicitly marked as a prerelease; it does **not** mean "stable or prerelease, whichever is newer." GitHub has no latest-prerelease endpoint or list filter, so list releases newest-first and scan client-side for the first `prerelease: true` item. Gitea also has no latest-prerelease endpoint, but its releases list supports `pre-release=true`; request that filter and take the first result. These paginated lookups use the same maximum page size and **5-page** cap as monorepo scans. GitLab releases have no corresponding prerelease field, so `--channel prerelease` is unsupported for GitLab sources and fails with a clear error.
   * When both a channel and monorepo selector apply, a candidate must satisfy both. GitHub filters both properties during the same client-side release scan. Gitea applies `pre-release=true` server-side when applicable and filters the tag prefix client-side. GitLab tag-prefix search is available only for its supported `stable` channel.

   Asset names are matched case-insensitively. Reject signatures, checksums, and source archives. A candidate must contain a recognized host OS marker and architecture marker at a non-alphanumeric boundary, and must end in a supported archive suffix, a recognized platform suffix, the release tag, or no extension. Rank the remaining candidates as follows:
   * Add 10 points for a supported archive suffix.
   * On Linux, add 5 points for a `static` marker.
   * When the host libc is known, add 20 points for the matching libc (`glibc`/`gnu` or `musl`) and subtract 1 point for the incompatible libc. An unmarked build therefore ranks above an explicitly incompatible one.
   * Linux-specific libc and static markers do not affect macOS ranking.
   Candidates are attempted in descending score order until one yields at least one compatible executable. If all candidates fail, installation reports the failure associated with each candidate.
2. **Idempotency / re-install / update semantics:**
   * A forge tag or canonical release-download URL automatically pins the selected version. `--unpin` overrides this source-derived pinning.
   * If no package with this ID is currently tracked, this is a fresh install: proceed to download using the requested or source-derived pin policy and requested channel (defaulting to tracking/`stable`), and record them.
   * If a package with this ID is already tracked:
     * Without `--reinstall`: this behaves like a **targeted update** for just this package. Existing explicit policy is preserved, but selecting an exact tag is itself an automatic-pinning operation: replacing a tracking installation with an exact version leaves it pinned unless `--unpin` was explicitly supplied. If the existing package is already pinned, or if the resolved version matches `current_version`, do nothing. Otherwise, proceed to install the selected version. This is what makes `eget install x; eget install x` idempotent-then-updating: the second invocation checks for and applies any update, but does no work if nothing changed.
     * With `--reinstall`: force re-download and re-linking even if the resolved version is unchanged, and additionally allow `--pin`/`--channel` (if passed) to overwrite the package's stored settings, exactly as `mark` would.
   * `-p`/`--ignore-existing` skips an already installed resolved package ID. It does not alter package metadata or files. Source resolution may still be required to determine the ID.
   * Without `--reinstall`, passing an explicit `--pin`, `--unpin`, or `--channel` for an existing package is rejected; use `mark` or reinstall. On reinstall, omitted pin and channel options preserve their stored values, while explicit values replace them.
3. **Staging download:** download the asset into a fresh staging directory `{$packageFilesDir}/tmp-{$randomId}`, never directly into the package's final `applicationIdHash` directory. This keeps a partially-downloaded/extracted package from ever being visible as "installed" if the process is interrupted.
4. **Extraction:** unpack the downloaded artifact into the staging directory. Supported containers are 7z, ZIP, tar, and tar compressed with gzip, bzip2, xz, or Zstandard. A standalone gzip, bzip2, xz, or Zstandard stream expands to one executable named `app`. An unrecognized format is treated as a plain executable, copied under `app` with mode `0755`; the download filename is not retained.
   * Every archive path must be relative and remain within the extraction root. Absolute paths, parent traversal, writes through archive-created symlinks, escaping link targets, conflicting entry types, special files, encrypted ZIP entries, and unsupported 7z entry types are rejected.
   * A containment walk after extraction rejects symlinks that resolve outside the extraction root.
   * File permissions supplied by the archive are preserved, except set-user-ID, set-group-ID, and sticky bits are removed. Ownership and extended attributes are not restored.
5. **Descending into the main directory:** if extraction produced a single wrapping directory containing no non-document files beside one subdirectory, descend into it repeatedly. Files named `install`, names containing `readme`, `license`, or `changelog`, and files ending in `.txt`, `.md`, or `.rst` are treated as documentation for this purpose. Stop before descending into a directory named `bin`.
6. **Binary discovery:** within the resolved directory (or its `bin/` subdirectory, if present), inspect only immediate entries. `eget` supports Linux and macOS payloads:
   * Parse candidate files as **ELF** on Linux or **Mach-O** on macOS, including fat/universal Mach-O binaries.
   * Confirm the parsed file is actually an **executable** (not a shared object/library) by checking the format's own executable-type field (e.g. ELF `e_type == ET_EXEC/ET_DYN-with-entry-point`, Mach-O `MH_EXECUTE`) rather than only pattern-matching the filename extension.
   * Confirm the binary's declared architecture (ELF `e_machine`, Mach-O `cputype`) matches the host architecture, rather than relying purely on filename tokens.
   * Accept shebang scripts only when their interpreter is an absolute path, or when they use `/usr/bin/env` followed by an interpreter name. Exclude filenames beginning with `install`, case-insensitively.
   * Exclude shared-library-looking names (`.so` with optional numeric suffixes, `.dll`, and `.dylib`) regardless of parsed format.
   * For archive formats that preserve executable modes, require at least one execute bit. Symlink candidates must resolve inside the extracted package root and satisfy the same checks as regular files.
   * Duplicate discovered command names reject the candidate. If no compatible executable is found, try the next ranked asset; fail installation if no asset succeeds.
7. **Command naming:** after extraction, if exactly one binary was discovered and its filename contains a delimiter (`.`, `_`, or `-`) followed by a recognized host-OS token, rename that binary to `app`. This replaces the entire original filename: with `app = 'foo'`, both `foo-linux` and `foo-linux-amd64` become `foo`. Do not apply this automatic rename when multiple binaries were discovered or when a filename contains only an architecture token. Then apply each `--rename FROM=TO` rule in order. Every `FROM` must identify a discovered binary, and a `TO` must not collide with another discovered path or existing entry. Rename rules are stored and reapplied on update and reinstall.
8. **Promotion into place:** move (rename) the staging directory's resolved contents from `tmp-{$randomId}` into `{$packageFilesDir}/{applicationIdHash}`, replacing any previous contents for this package ID (if updating). This should be an atomic directory rename where the filesystem allows it.
9. **Symlinking:** for each discovered binary, create a symlink in the target `$binDir` pointing to its absolute path under `installation_dir`. A command path belonging to the package being replaced may be updated. Any other existing filesystem entry causes installation to fail unless `--force` was supplied. On update or reinstall, previously recorded command links absent from the new binary set are removed so dropped commands do not leave stale links.
10. **Metadata commit:** within a single DB transaction, replace the `packages` row and its `binaries` rows with the newly prepared state, including rename rules and HTTP validators. `updated_at` and other change metadata are written only after an actual install/update; a no-op leaves the row untouched. Commit only after directory promotion and symlink creation succeed.
11. **Rollback and cleanup:** before activation, move the old package directory and affected command paths into the staging tree as backups. If promotion, link creation, or the DB transaction fails, remove newly created links and restore every backup. After a successful DB commit, temporary candidates, backups, and obsolete package contents are removed with the staging directory.

Command names are exclusive filesystem resources. Installation never replaces a conflicting path implicitly; `--force` is required even when the conflicting entry is an untracked symlink or regular file. Update does not imply `--force` for newly introduced command names.

### Direct-URL version tracking

Forge-hosted packages have a natural notion of "version" (the release tag). Plain `direct`-kind packages generally don't — a bare URL like `https://dl.min.io/aistor/mc/release/linux-amd64/mc` has no version string anywhere, only bytes that may or may not have changed. `eget` supports two modes for tracking a direct package's version:

**Without `--version-url`** (the default): there is no authoritative version string, so `current_version` is `NULL`. `eget update`/re-running `install` detects a change by issuing a `HEAD` request against the installed asset URL and comparing the returned `ETag`/`Last-Modified` against the stored `etag`/`last_modified`. If the server doesn't properly support `HEAD`, `eget` falls back to issuing a `GET` instead but terminates the connection as soon as the response headers have been read, without waiting for (or discarding) the body — the `ETag`/`Last-Modified` headers are available before any body bytes arrive either way, so there's no need to actually download the asset just to check whether it changed. If either has changed, the asset is re-downloaded and reinstalled; otherwise nothing happens.

A direct URL whose normalized path contains a version-like numeric core is automatically pinned because such a URL normally identifies an immutable release rather than a moving download. The numeric core consists of three non-empty ASCII digit components separated by dots (`digits.digits.digits`); it must be preceded by the start of the path or a non-digit and followed by the end of the path or `.`, `-`, `_`, or `/`. Only the path is inspected, not the host, query, or fragment. Thus `https://go.dev/dl/go1.25.0.linux-amd64.tar.gz` and `https://cache.agilebits.com/dist/1P/op2/pkg/v2.35.0/op_linux_amd64_v2.35.0.zip` are automatically pinned. This heuristic does not populate `current_version`. An explicit `--pin` or `--unpin` overrides heuristic pinning, subject to the validator rule below. It is not applied when `--version-url` supplies an authoritative tracking mechanism.

If the download response provides neither `ETag` nor `Last-Modified`, the package is automatically pinned because there is no signal with which to detect an update. This download-time rule also applies to reinstalls and overrides an install-time `--unpin`, with a warning explaining why the package could not remain tracking. An explicit `eget mark --unpin` remains allowed; updates for such a package are skipped with `no HTTP validators` until it is reinstalled from a response that provides a validator or configured with `--version-url`. Existing package records are not migrated automatically.

**With `--version-url <version-url>`**, e.g. `eget install <url> --version-url <version-url>`: this lets a direct package have an actual version string, resolved from a separate endpoint, and requires the download `<url>` itself to contain a `{{version}}` placeholder that gets substituted with whatever version is resolved. Validation at install time:
* The `<url>` being installed must contain the literal substring `{{version}}`; otherwise the command fails immediately (there would be nothing for `--version-url` to parameterize).
* `--version-url` may only be used when installing a single package in that invocation — passing it alongside multiple locrefs/package specs in one `eget install` call is rejected.

**Resolving the version**, both at install time and on every subsequent `update`:
1. `GET` the `version-url`.
2. Require a `Content-Type` response header. Media-type matching is case-insensitive and permits parameters such as `charset=utf-8`. `application/json` and any `application/*+json` media type are treated as JSON; `text/plain` is treated as plain text. A missing, malformed, or different media type fails the install/update.
3. For a JSON response, apply the regex `/"(version|latest)"\s*:\s*"[^"]+"/` against the raw response body and take the first match's captured string value as the version. The body does not need to be fully parsed as JSON, but a response without a matching field fails instead of falling back to plain-text handling.
4. For a `text/plain` response, trim each line, skip lines that are empty after trimming, and use only the first remaining line. Later lines are ignored. A response with no non-empty line fails.
5. Strip leading/trailing whitespace from the extracted string, then validate it: it must be non-empty and no more than 64 bytes (a real version string won't be longer than that). If validation fails, the install/update fails with an error.
6. Substitute the resolved version into the `{{version}}` placeholder(s) in the stored download URL template to get the concrete asset URL to download.

`etag` and `last_modified` store validators returned by `version_check_url` when an install or update is committed. Every check still resolves the version with `GET`; only a changed version string triggers a re-download. A validator change with the same resolved version is a no-op.

### Update

`eget update [packageId...]` re-checks either the given package IDs or, if none given, every tracked package:

* Pinned packages are skipped entirely (reported as "skipped: pinned").
* For forge-hosted packages, the [monorepo-aware release selection](#monorepo-detection-forge-hosted-packages) lookup is re-run against the package's stored `channel`/`release_selector`, and compared against `current_version`.
* For `direct`-kind packages, version change is detected per [Direct-URL version tracking](#direct-url-version-tracking) above — either a `HEAD` on the asset URL (no `--version-url`), or a `version-url` re-check (if one was set at install time).
* While packages are being checked, a progress bar reports the number completed. After all probes finish, the transient bar is cleared and eget prints the skipped packages with their reasons followed by every updatable package. Forge packages and versioned direct URLs include their current and selected versions; validator-only direct URLs have no version labels. Unchanged packages are omitted from this summary. Confirmation is requested only after this list is visible.
* If nothing has changed, the package is left completely untouched — in particular, **`updated_at` is only bumped when an update actually happens**, not on a no-op check (it is a "last actually changed" timestamp, not a "last checked" timestamp).
* If changed, the same download → extract → discover binaries → stage → symlink → commit flow described in [Installation](#installation) (steps 3–11) is followed, reusing the package's existing `channel`/`pinned`/`bin_dir`/`release_selector` settings. As part of symlinking (step 9), any binary that the *previous* version provided but the *new* version no longer does has its stale symlink removed from `bin_dir`.

With `-y`/`--assume-yes`, all available updates are applied without prompting. With `--assume-no`, resolution and reporting still occur but no update is applied. The flags conflict. Before applying a confirmed update, `eget` re-reads the package record and aborts that package if its stored state differs from the state that was probed.

### Uninstallation

Uninstall requires an exact installed package ID. If a slashless value is not installed but exactly matches one or more recorded binary names, the error reports the owning package IDs as suggestions; it does not remove them automatically.

Uninstalling an installed package involves:
1. While the package directory still exists, identify each recorded command path that is still a symlink whose canonical target is an existing descendant of the package's canonical installation directory. Broken links and links redirected outside the package are treated as user-modified and preserved.
2. Create an empty quarantine directory and start an immediate metadata DB transaction.
3. Delete the package's `packages` row inside the transaction; its `binaries` rows are deleted by the foreign-key cascade. The deletion remains uncommitted during the filesystem phase.
4. Unlink every package-owned symlink identified in step 1 from `bin_dir`.
5. Move the package contents from `{$packageFilesDir}/{applicationIdHash}` into the quarantine directory. Ownership must have been checked in step 1 because this rename makes any remaining links dangling.
6. Commit the metadata transaction only after all link removals and the directory rename succeed.
7. If deletion, unlinking, moving, or commit fails, roll back the transaction, restore the quarantined package directory, and recreate every already-removed symlink with its original target.
8. Permanently remove the quarantined contents after the DB commit succeeds.

The SQLite transaction and compensating filesystem restoration cover ordinary operation failures, but they cannot make SQLite and filesystem changes jointly crash-atomic. Crash recovery would require a persistent operation journal; temporary quarantine directories alone are not such a journal.

### Mark

`eget mark [--pin | --unpin] [--channel stable|prerelease] <packageId...>` updates the stored `pinned`/`channel` columns for already-tracked packages directly, without touching files, downloads, or symlinks. At least one policy option is required, and `--pin` conflicts with `--unpin`/`--no-pin`. This is the supported way to change package policy outside `install --reinstall`. Setting `--channel prerelease` on a GitLab package is rejected because GitLab's Releases API has no prerelease classification to preserve or query later.

### List

`eget list` enumerates all rows in the `packages` table (optionally filtered to a given package ID prefix or owner) and prints one line per package as a plain tab-separated list of: the package ID, `current_version` (`-` when null), the tracking/pinned state, and a comma-separated list of that package's installed binary names (from the `binaries` table). For example:

```
github.com/BurntSushi/ripgrep	14.1.0	tracking	rg
github.com/supriyo-biswas/static-builds:gnu-sed	4.1	pinned	sed
```
