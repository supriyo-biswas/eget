# Scopes and storage

Related specifications: [overview](eget.md), [identity and state](identity-and-state.md),
and [installation](installation.md).

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

The default values of these variables depend on the scope, determined by the environment variable `EGET_SCOPE`, the global flag `--scope`, or automatic project-scope discovery. A scope is one of `system`, `user`, or `project`.

For root users, the default scope is `system`; they may explicitly choose `user` or `project`. For non-root users, an unspecified scope first attempts project discovery and falls back to `user`; `system` is disallowed. Explicit `user` or `system` selection bypasses project discovery. Explicit `project` selection requires discovery to succeed rather than falling back.

For project discovery, canonicalize the current directory and `HOME`, then inspect the current directory and each parent in turn. Stop without inspecting a directory when it is canonical `HOME` or its owner differs from the effective UID. The first owned directory containing a regular, non-symlink `eget-packages.txt` is the project root. Thus a marker directly in `HOME` is invalid, while the nearest marker below it wins. Root follows the same ownership rule when project scope is explicitly requested, so only root-owned directories are considered. The marker is also the project package manifest described below.

The following paths are used. XDG variables use their FreeDesktop fallbacks when unset; the user lock falls back from `$XDG_RUNTIME_DIR` to `$XDG_DATA_HOME`.

|              | Metadata DB                                  | Lock file                        | Package files                       | Binary links             |
| ------------ | -------------------------------------------- | -------------------------------- | ----------------------------------- | ------------------------ |
| System scope | `/var/lib/eget/eget/eget.sqlite3`            | `/run/lock/eget.lock`            | `/opt/eget/<applicationIdHash>`     | `/usr/local/bin`         |
| User scope   | `$XDG_DATA_HOME/eget/eget.sqlite3`           | `$XDG_RUNTIME_DIR/eget.lock`      | `$XDG_DATA_HOME/eget/<applicationIdHash>` | `$HOME/.local/bin` |
| Project scope | `<project>/.eget/eget.sqlite3`              | `<project>/.eget/eget.lock`       | `<project>/.eget/<applicationIdHash>` | `<project>/.eget/bin`  |

`local` is not accepted as an alias for `project`. `EGET_LOCAL_DATA_DIR`, `EGET_LOCAL_LOCK_DIR`, `EGET_LOCAL_PKG_DIR`, and `EGET_LOCAL_BIN_DIR` are not used, and there is no migration from scopes previously configured through those variables. If the selected directories do not exist, `eget` creates them when an operation prepares the scope.

The symlink directory may be overridden per-invocation of `install` when in `user` or `system` scope, via the `--to` flag, or the `EGET_BIN_DIR`/`EGET_BIN` environment variables. If more than one is set, precedence is `--to` > `EGET_BIN_DIR` > `EGET_BIN` > the scope's default `$binDir`. These overrides are disallowed in `project` scope.

The selected `bin_dir` is stored per package. Later `update` operations retain it. A repeated `install` without a destination override also retains it; if an override is present and that invocation performs an installation, the package is relinked into the selected directory.

At user scope, the default `$stateDir`/`$packageFilesDir` resolve under `$XDG_DATA_HOME` (per the table above), so a package's on-disk contents live at `$XDG_DATA_HOME/eget/<applicationIdHash>` (commonly `~/.local/share/eget/<applicationIdHash>`, since `$XDG_DATA_HOME` defaults to `~/.local/share`). `<applicationIdHash>` is defined in [Package/application IDs](identity-and-state.md#packageapplication-ids).

## Locking

Every package management command (`install`, `list`, `update`, `uninstall`, `mark`, `exec`) acquires an exclusive lock on `{$lockDir}/eget.lock` before touching the metadata DB or the filesystem. The first five commands hold it for their entire duration and release it on exit (including on error). `exec` holds it only while resolving and validating the executable, then closes the database and releases the lock before process replacement so a long-running command does not block package operations. A concurrent removal after this point may cause process replacement to fail cleanly.

If the lock is already held, `eget` does not fail immediately. It prints a user-visible attempt counter and makes 10 attempts, waiting one second between attempts (at most nine seconds of deliberate waiting). If the lock still cannot be acquired on the 10th attempt, `eget` exits with an error.
