# Package lifecycle

Related specifications: [overview](eget.md), [installation](installation.md),
[source resolution](source-resolution.md), and [identity and state](identity-and-state.md).

## Direct-URL version tracking

Forge-hosted packages have a natural notion of "version" (the release tag). Plain `direct`-kind packages generally don't — a bare URL like `https://dl.min.io/aistor/mc/release/linux-amd64/mc` has no version string anywhere, only bytes that may or may not have changed. `eget` supports two modes for tracking a direct package's version:

Explicit direct download URLs may be MiniJinja templates. Output expressions accept both `{{x}}` and `{{ x }}` forms. Statement tags are limited to `{% if ... %}`, `{% elif ... %}`, `{% else %}`, and `{% endif %}`; loops, assignments, macros, includes, inheritance, and template comments are rejected. Templates receive `kernel` (`linux` or `darwin`), `arch` (`x86_64` or `aarch64`), and, when `--version-url` is configured, `version`. No other variables are available. Every output expression must evaluate to a non-empty string; undefined values, `none`, empty strings, and non-string results fail rendering. A conditional statement with no selected branch may emit nothing. The final rendering must pass normal direct-URL and embedded-credential validation.

Template syntax, statement names, and referenced variables are validated before network access. The original template is stored and rendered again for reinstalls and updates, including before validator-based `HEAD` or fallback `GET` requests. Templates apply only to explicit direct URLs: forge locrefs, `--version-url` itself, and URLs discovered in forge release bodies are never evaluated as templates.

**Without `--version-url`** (the default): there is no authoritative version string, so `current_version` is `NULL`. `eget update`/re-running `install` detects a change by issuing a `HEAD` request against the installed asset URL and comparing the returned `ETag`/`Last-Modified` against the stored `etag`/`last_modified`. If the server doesn't properly support `HEAD`, `eget` falls back to issuing a `GET` instead but terminates the connection as soon as the response headers have been read, without waiting for (or discarding) the body — the `ETag`/`Last-Modified` headers are available before any body bytes arrive either way, so there's no need to actually download the asset just to check whether it changed. If either has changed, the asset is re-downloaded and reinstalled; otherwise nothing happens.

A direct URL whose normalized path contains a version-like numeric core is automatically pinned because such a URL normally identifies an immutable release rather than a moving download. The numeric core consists of three non-empty ASCII digit components separated by dots (`digits.digits.digits`); it must be preceded by the start of the path or a non-digit and followed by the end of the path or `.`, `-`, `_`, or `/`. Only the path is inspected, not the host, query, or fragment. Thus `https://go.dev/dl/go1.25.0.linux-amd64.tar.gz` and `https://cache.agilebits.com/dist/1P/op2/pkg/v2.35.0/op_linux_amd64_v2.35.0.zip` are automatically pinned. This heuristic does not populate `current_version`. An explicit `--pin` or `--unpin` overrides heuristic pinning, subject to the validator rule below. It is not applied when `--version-url` supplies an authoritative tracking mechanism.

If the download response provides neither `ETag` nor `Last-Modified`, the package is automatically pinned because there is no signal with which to detect an update. This download-time rule also applies to reinstalls and overrides an install-time `--unpin`, with a warning explaining why the package could not remain tracking. An explicit `eget mark --unpin` remains allowed; updates for such a package are skipped with `no HTTP validators` until it is reinstalled from a response that provides a validator or configured with `--version-url`. Existing package records are not migrated automatically.

**With `--version-url <version-url>`**, e.g. `eget install <url> --version-url <version-url>`: this lets a direct package have an actual version string, resolved from a separate endpoint, and exposes the resolved string as `version` while rendering the download URL template. Validation at install time:
* The download URL template must reference `version`; conversely, a template that references `version` requires `--version-url`. Both mismatches fail before making a request.
* `--version-url` may only be used when installing a single package in that invocation — passing it alongside multiple locrefs/package specs in one `eget install` call is rejected.

**Resolving the version**, both at install time and on every subsequent `update`:
1. `GET` the `version-url`.
2. Require a `Content-Type` response header. Media-type matching is case-insensitive and permits parameters such as `charset=utf-8`. `application/json` and any `application/*+json` media type are treated as JSON; `text/plain` is treated as plain text. A missing, malformed, or different media type fails the install/update.
3. For a JSON response, apply the regex `/"(version|latest)"\s*:\s*"[^"]+"/` against the raw response body and take the first match's captured string value as the version. The body does not need to be fully parsed as JSON, but a response without a matching field fails instead of falling back to plain-text handling.
4. For a `text/plain` response, trim each line, skip lines that are empty after trimming, and use only the first remaining line. Later lines are ignored. A response with no non-empty line fails.
5. Strip leading/trailing whitespace from the extracted string, then validate it: it must be non-empty and no more than 64 bytes (a real version string won't be longer than that). If validation fails, the install/update fails with an error.
6. Render the stored download URL template with the resolved `version` and current host values to get the concrete asset URL to download.

`etag` and `last_modified` store validators returned by `version_check_url` when an install or update is committed. Every check still resolves the version with `GET`; only a changed version string triggers a re-download. A validator change with the same resolved version is a no-op.

## Update

`eget update [packageId...]` re-checks either the given package IDs or, if none given, every tracked package:

* Pinned packages are skipped entirely (reported as "skipped: pinned").
* For forge-hosted packages, the [monorepo-aware release selection](source-resolution.md#monorepo-detection-forge-hosted-packages) lookup is re-run against the package's stored `channel`/`release_selector`, and compared against `current_version`.
* For `direct`-kind packages, version change is detected per [Direct-URL version tracking](#direct-url-version-tracking) above — either a `HEAD` on the asset URL (no `--version-url`), or a `version-url` re-check (if one was set at install time).
* While packages are being checked, a progress bar reports the number completed. Update-check and asset-download progress bars draw to stderr with a maximum ordinary refresh rate of four renders per second (one redraw per 250 ms); completion and clearing may render immediately. After all probes finish, the transient bar is cleared and eget prints the skipped packages with their reasons followed by every updatable package. Forge packages and versioned direct URLs include their current and selected versions; validator-only direct URLs have no version labels. Unchanged packages are omitted from this summary. Confirmation is requested only after this list is visible.
* If nothing has changed, the package is left completely untouched — in particular, **`updated_at` is only bumped when an update actually happens**, not on a no-op check (it is a "last actually changed" timestamp, not a "last checked" timestamp).
* If changed, the same download → extract → discover binaries → stage → symlink → commit flow described in [Installation](installation.md#installation) (steps 3–11) is followed, reusing the package's existing `channel`/`pinned`/`bin_dir`/`release_selector` settings. As part of symlinking (step 9), any binary that the *previous* version provided but the *new* version no longer does has its stale symlink removed from `bin_dir`.

With `-y`/`--assume-yes`, all available updates are applied without prompting. With `--assume-no`, resolution and reporting still occur but no update is applied. The flags conflict. Before applying a confirmed update, `eget` re-reads the package record and aborts that package if its stored state differs from the state that was probed.

## Uninstallation

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

A successful removal prints `Uninstalled <packageId> in <scope>`, using the same project-path formatting as installation. In project scope, the matching manifest entry is removed after the package mutation succeeds. Failed removals retain their existing output and manifest entry.

## Mark

`eget mark [--pin | --unpin] [--channel stable|prerelease] <packageId...>` updates the stored `pinned`/`channel` columns for already-tracked packages directly, without touching files, downloads, or symlinks. At least one policy option is required, and `--pin` conflicts with `--unpin`/`--no-pin`. This is the supported way to change package policy outside `install --reinstall`. Setting `--channel prerelease` on a GitLab package is rejected because GitLab's Releases API has no prerelease classification to preserve or query later. In project scope, a matching manifest entry is rewritten from the resulting stored policy; an installed package absent from the manifest is not added.

## List

`eget list` enumerates all rows in the `packages` table (optionally filtered to a given package ID prefix or owner) and prints one line per package as a plain tab-separated list of: the package ID, `current_version` (`-` when null), the tracking/pinned state, and a comma-separated list of that package's installed binary names (from the `binaries` table). For example:

```
github.com/BurntSushi/ripgrep	14.1.0	tracking	rg
github.com/supriyo-biswas/static-builds:gnu-sed	4.1	pinned	sed
```

## Exec

`eget exec [-p|--package PACKAGE_ID] [--] COMMAND [ARG...]` (alias `eget x`) executes a command recorded in the active scope. Scope selection follows the normal `--scope`/`EGET_SCOPE`/default rules and does not search or combine other scopes. Arguments after `COMMAND` are passed through without further option parsing; `--` may be used before `COMMAND` when needed.

Without `--package`, `eget` looks up `COMMAND` in the `binaries` table. No match is an error. One match selects that package. If multiple packages provide the same name, `eget` lists every owning package ID in deterministic order and asks the user to rerun `eget x -p <PACKAGE_ID> COMMAND`. With `--package`, the value must be an exact installed package ID and that package must record `COMMAND`; package-ID prefixes are not accepted.

The executable is resolved from the selected package's stored `bin_dir`, never from the ambient `PATH`. The recorded command path must be a live symlink whose canonical target is an existing regular file strictly beneath the package's canonical `installation_dir`. Missing, broken, replaced, or out-of-package links are rejected. Because the exact original internal target is not stored, containment within the selected package is the strongest available integrity check.

After resolution, `eget` releases its package lock and uses Unix process replacement rather than spawning a child. The command receives its requested name as `argv[0]` where the operating system permits, plus every supplied argument. It inherits the caller's current directory, environment, and standard streams without modifying `PATH`; its exit status and signal behavior therefore become those of the `eget` invocation.
