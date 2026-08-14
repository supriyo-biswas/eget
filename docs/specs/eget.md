# eget specification

This is the specification for the `eget` CLI, a package manager that installs
standalone executables from GitHub, Gitea, GitLab, and direct URLs.

## System overview

`eget` provides six operations:

- `install` resolves a package or URL, downloads and extracts an artifact,
  exposes compatible commands, and records the installation.
- `update` refreshes tracking installations while respecting stored release
  and pinning policy.
- `uninstall` removes package contents and links still owned by the package.
- `mark` changes stored user policy such as pinning and release channel.
- `list` displays installed package metadata.
- `exec` runs an installed command selected by command name and, optionally,
  exact package ID.

The `install` subcommand may be omitted when the command-position argument
contains `/`, so `eget owner/project` means `eget install owner/project`.
Commands accepting multiple packages process every operand and return a
non-zero status if any operand fails.

At a high level, installation resolves the user's locref into a package and
ranked assets, downloads candidates into staging, safely extracts one, exposes
its compatible commands through managed links, and transactionally commits
the package metadata. Package-management commands serialize state changes with
a scope-specific lock; `exec` releases that lock before process replacement.

## Detailed specifications

- [Scopes and storage](scopes-and-storage.md) defines scope selection,
  filesystem locations, project discovery, destination overrides, and locking.
- [Identity and state](identity-and-state.md) defines package IDs, application
  IDs and hashes, database records, migrations, and compatibility handling.
- [Source resolution](source-resolution.md) defines authentication, locref
  probing, forge behavior, application-name derivation, and release selection.
- [Installation](installation.md) defines manifests, candidate selection,
  extraction, binary discovery, naming, promotion, managed links, and rollback.
- [Package lifecycle](package-lifecycle.md) defines direct-version checks,
  updates, reinstall behavior, uninstall, mark, list, and exec.
- [AppImage](appimage.md) defines the optional Linux `extras` feature's
  AppImage extraction, launcher, command naming, and desktop-entry behavior.

Supporting investigations and decisions live under
[`docs/decisions`](../decisions/). They explain why the normative behavior was
chosen but do not override these specifications.

## Terminology and precedence

A **locref** is an install input: a direct URL, package ID, or application ID.
A **package ID** identifies the source and application and may include a
monorepo selector. An **application ID hash** determines the package's stable
on-disk directory and AppImage desktop filename. These are defined precisely
in [Identity and state](identity-and-state.md).

When documents overlap, the format-specific specification controls only its
explicit exceptions. General installation, transaction, scope, and lifecycle
rules continue to apply.
