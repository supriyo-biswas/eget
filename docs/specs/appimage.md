# AppImage specification

Related specifications: [overview](eget.md), [installation](installation.md),
[package lifecycle](package-lifecycle.md), and the supporting
[extraction investigation](../decisions/appimage-extraction.md).

This document is normative for AppImage-specific behavior. General candidate,
transaction, scope, update, and uninstall rules remain defined by the linked
core specifications.

AppImage support is available only on Linux when eget is built with the Cargo
feature `extras`. Official releases keep the lean binaries under
`eget-linux-<arch>` and publish AppImage-capable binaries as
`eget-linux-<arch>-extras`. Without the feature, eget applies its ordinary
pre-AppImage asset selection and plain-executable behavior; it does not inspect
AppImage magic or emit a feature-specific diagnostic.

## Dependencies

Use in-process, read-only readers:

- [`backhand`](https://docs.rs/backhand/0.25.1/backhand/) for SquashFS 4.0.
  Disable its default features and enable only `gzip` and `zstd`, the codecs
  present in the [investigated AppImages](../decisions/appimage-extraction.md#investigation). This avoids its XZ, LZ4, LZO, and
  Rayon dependencies while sharing eget's existing `flate2` and `zstd` graph.
- [`dwarfs`](https://docs.rs/dwarfs/0.2.1/dwarfs/) for DwarFS. Disable its
  default features and enable only `zstd`, which shares eget's existing
  `zstd-safe`/`zstd-sys`; construct it at the computed image offset. Version 0.2.1
  understands DwarFS filesystem versions 2.3 through 2.5, including the 2.5
  images documented in the [investigation](../decisions/appimage-extraction.md#investigation). Validate against real newest-publisher images because
  the crate only guarantees compatibility with the upstream versions named in
  its own documentation.

Do not invoke filesystem utilities from `src/archive.rs`. External utilities
make installation host-dependent and bypass the archive safety checks that
`eget` already owns.

## Source selection and naming

When `extras` is enabled on Linux, add `.appimage` case-insensitively to all
artifact-suffix logic in `src/source.rs`:

1. It is a supported terminal release-asset suffix and receives the same
   archive score as other extractable package formats.
2. It is stripped during direct-URL application-name derivation.
3. `.AppImage.zsync`, signatures, and checksums remain rejected.

On Linux, the `.AppImage` suffix itself is a sufficient OS marker, so generic
publisher names such as `OpenOffice.AppImage` remain installable during
repository scans. If an AppImage name contains a recognized architecture
marker, it must match the host. On macOS no AppImage candidate is selected;
AppImage is a Linux format.

Detection must also be content-aware. Refactor `archive::extract` to inspect
the downloaded payload and return its detected format. A name ending in
`.AppImage` whose content is not a valid AppImage is an error, rather than a
plain executable. An extensionless direct download with valid `AI02` magic may
be recognized as an AppImage. Other extensionless files retain the existing
plain-executable behavior.

## Envelope parsing

Add `Format::AppImage` and an `src/appimage.rs` module. Parsing is read-only and
uses checked arithmetic throughout:

1. Read enough bytes for the ELF identification and AppImage magic. Require a
   supported little-endian 32- or 64-bit ELF executable with `AI02` at offset
   8. Validate the file length before every range read.
2. Parse the required ELF header, program-header, and section-header fields
   directly. Compute the end of the ELF runtime as the maximum of:
   - the end of the section-header table;
   - every file-backed section's `sh_offset + sh_size`; and
   - every file-backed program segment's `p_offset + p_filesz`.
   Ignore `SHT_NOBITS` section sizes because they occupy no file bytes. This is
   the payload offset; do not scan forward for a magic string.
3. At the computed offset, classify `hsqs` as little-endian SquashFS and
   `DWARFS\x02\x03` through `DWARFS\x02\x05` as a supported DwarFS version.
   Pass the same bounded file and offset to the selected reader, which must
   validate the complete superblock and filesystem bounds.
4. Reject `AI01` with a clear “type-1 AppImage is not supported” diagnostic.
   Reject type 0, unknown type bytes, unknown filesystems, unsupported DwarFS
   versions, malformed ELF layouts, overflows, and truncated images.

Use the native image-offset constructors provided by both readers:
`backhand`'s offset constructor for SquashFS and DwarFS `SectionReader`'s
archive offset. Both translate filesystem-relative reads with checked
arithmetic and do not interpret bytes before the payload as filesystem data.

## Safe materialization

Both backends apply the same materialization policy to their native entry
models. Extraction follows the existing security policy in `src/archive.rs`:

1. Walk and validate the complete image before writing payload entries.
   Component names must be non-empty single relative components; reject `/`,
   NUL, `.`, `..`, duplicate/conflicting paths, and arithmetic or size
   inconsistencies.
2. Accept only directories, regular files, and symlinks. Reject devices,
   sockets, and FIFOs. Validate every symlink target lexically with the existing
   `validate_target`; absolute or escaping targets fail the candidate.
3. Create directories first, stream regular-file contents second, and create
   validated symlinks last. This prevents writes through archive-created
   links. Preserve ordinary permission bits, remove set-ID/sticky bits, and do
   not restore ownership or xattrs. Recreate hard links when the backend
   exposes a stable inode identity; otherwise materialize an equivalent regular
   file.
4. Apply directory modes deepest-first after their children have been written,
   then run the existing containment walk.
5. Require an exact root entry named `AppRun`. It must be executable and must
   be a host-compatible ELF executable or an accepted shebang script; a
   symlink must resolve to a regular file inside the extracted root.

The extracted AppDir root is always the package root. Do not call
`compat::descend_single_root`: AppImage root entries and paths have defined
meaning even if the tree happens to contain one directory.

Set conservative DwarFS metadata and block-cache limits instead of its large
defaults, and stream files into the staging tree rather than collecting them
in memory. Any future global extracted-size limit should apply equally to all
archive formats rather than giving AppImage a separate policy.

## Command mapping and launcher

Use explicit prepared-command mappings rather than treating every command as a
physical binary rename:

```text
PreparedCommand {
    name: String,          // recorded command and link name
    target: PathBuf,       // path below Prepared.root
}
```

For existing formats, both values initially come from the discovered binary
and current physical rename semantics stay unchanged. For an AppImage, create
exactly one mapping using the logical name derived below. Its target is an
`eget`-generated executable launcher stored at a reserved path in the AppDir.
Do not discover or expose the many other executables in the image.

The launcher must:

1. set `APPDIR` to the final absolute installation directory;
2. set `OWD` to the invocation working directory and `ARGV0` to the invoked
   command path;
3. export those variables; and
4. `exec "$APPDIR/AppRun" "$@"` without changing directory.

Generate it as a mode-0755 `/bin/sh` script using the existing `shlex` support
to quote the final installation path. Reject an image that already owns the
reserved launcher path instead of overwriting package content.

The original downloaded AppImage is not retained after extraction, so the
launcher should leave `APPIMAGE` unset rather than point it at a misleading
path. This deliberately disables AppImage self-update and adjacent `.home` or
`.config` portable-mode conventions; `eget` owns update and storage policy.

`Prepared::binary_names`, `Prepared::links`, database `binaries` rows,
conflict detection, `exec`, update, rollback, and uninstall all operate on the
logical command name. The filesystem link is therefore:

```text
$binDir/<logical-command> -> <installation_dir>/.eget-appimage-launcher
                                      -> <installation_dir>/AppRun
```

For AppImages, derive the initial logical command from the normalized asset
filename. Remove the `.AppImage` suffix and recognized version/platform
suffixes, and use the result unless it is a generic `app`, `appimage`, or
`application`. For a generic asset name, use the repository application name
after stripping a terminal `-appimage`, `_appimage`, or `.appimage` when
present. Then apply `--rename FROM=TO` to that logical name only; it must not
rename `AppRun` or the generated launcher.

No database migration is required. The existing `binaries` table already
stores logical command names, while the managed symlink resolves beneath the
package installation directory as required by `exec` and uninstall.

## Desktop entry integration

The [AppImage specification](https://github.com/AppImage/AppImageSpec/blob/51c2a1465cfef1be7a159477ada8cc36a790e96c/draft.md#contents-of-the-image)
says an AppDir should contain exactly one root-level `$APPNAME.desktop` file
and should contain matching icons below `usr/share/icons/hicolor`, with a root
icon and `.DirIcon` as fallbacks. Treat that file as application metadata, not
as another executable candidate.

Desktop integration is scope-dependent:

| Scope | Desktop entry destination |
| --- | --- |
| Project | None. Keep the publisher's file inside the extracted package, but do not register it with the host desktop. |
| User | `$XDG_DATA_HOME/applications`, defaulting to `$HOME/.local/share/applications` when `XDG_DATA_HOME` is unset or empty. |
| System | `/usr/share/applications`; system scope already requires root. |

The user location follows the
[XDG Base Directory Specification](https://specifications.freedesktop.org/basedir/).
`/usr/share` is its default system data installation prefix. Create the
destination directory as needed, but never derive a host path from an untrusted
path or filename inside the image.

Do not publish the original desktop file verbatim. Parse it as UTF-8 according
to the
[Desktop Entry Specification](https://specifications.freedesktop.org/desktop-entry/latest-single/),
require one root-level regular file or safe in-tree symlink with
`[Desktop Entry]` and `Type=Application`, and generate a rewritten copy at the
reserved in-package path `.eget-appimage.desktop`. If there is no unambiguous
valid desktop file, command installation may still succeed and desktop
integration is skipped. A selected but malformed entry produces a warning;
the AppImage specification makes this metadata recommended rather than
mandatory.

The generated entry must preserve comments, localized values, unknown keys,
and application-action groups while making these controlled changes:

1. Rewrite the executable in the main `Exec=` value and every `[Desktop Action
   ...]` `Exec=` value to the final absolute managed command link
   `$binDir/<logical-command>`. Parse and serialize the desktop-entry command-line
   grammar instead of using shell quoting, and preserve the publisher's
   remaining arguments and field codes.
2. Set `TryExec` to the same absolute command link when that key is present.
   Desktop launchers do not necessarily inherit a shell's `PATH`.
3. Set `DBusActivatable=false`. `eget` installs neither the publisher's D-Bus
   service nor a desktop file under the publisher's D-Bus application ID, so
   activation must go through the rewritten `Exec` path and AppRun launcher.
4. Resolve `Icon=` against the extracted AppDir using the AppImage preference
   order: a matching file below `usr/share/icons/hicolor`, then a matching root
   SVG/SVGZ/PNG, then `.DirIcon`. When a safe image is found, rewrite `Icon` to
   its final absolute path inside the installed package. Do not copy icons into
   the host icon theme merely to make the desktop entry work; absolute icon
   paths are valid desktop-entry icon strings.

Register the generated file using a stable, collision-resistant desktop file
ID such as `eget-<applicationIdHash>.desktop`, rather than trusting the source
filename. The entry in the scope's `applications` directory should be a
managed symlink to `<installation_dir>/.eget-appimage.desktop`. This keeps all
generated content inside the package, gives updates a stable external path,
and permits the same ownership check used for command links: an entry is owned
only while it is a symlink resolving strictly beneath that package's
installation directory.

Generalize activation's managed-link set to include this desktop link. Check
conflicts before mutation, back up and replace it in the same transaction as
the package directory and command link, restore it on rollback, and remove it
on update to a non-AppImage or uninstall only when the ownership check still
passes. Because its filename and destination are deterministic from the scope
and application ID, it does not require a new database row. Renaming the
command rewrites `Exec` and `TryExec`, but does not change the desktop file ID.

Do not run the AppImage's own desktop-integration code. Invoking the embedded
runtime or application for integration would violate the extraction-only
design and would place files outside eget's transaction. Calling
`update-desktop-database` and registering MIME metadata or icons in shared
caches can be added separately if testing shows it is necessary.

## Errors and candidate fallback

AppImage failures participate in the existing candidate fallback. Diagnostics
should distinguish at least:

- invalid AppImage magic or malformed ELF runtime;
- unsupported type-1 AppImage;
- invalid payload offset or truncated payload;
- unsupported embedded filesystem/version/compression;
- unsafe filesystem entry or link;
- missing, non-executable, escaping, or host-incompatible root `AppRun`.

The downloaded runtime is never executed, even as a fallback. A release with
an invalid AppImage may fall through to another ranked asset just like a bad
ZIP or tar candidate.

## Tests and fixtures

The committed deterministic type-2 SquashFS fixture under
`tests/fixtures/archives` contains a root `AppRun`, nested payload, executable
modes, a safe relative symlink, desktop metadata, and icons. Unit tests cover
ELF offset calculation, false filesystem magic, type-1 identification, unsafe
paths, and links. An opt-in `EGET_TEST_APPIMAGE` test validates either backend
against a real publisher image; both SquashFS and DwarFS publisher images were
used during implementation.

Further backend fixtures can cover:

- a compact DwarFS 2.5 image and hard-link identity;
- AppRun variants for a script, ELF, and symlink;
- malformed envelopes: `AI01`, bad `AI02`, truncated section tables, overflowed
  ranges, a false filesystem magic inside the runtime, unknown payload magic,
  and unsupported DwarFS version;
- unsafe filesystem trees: absolute/parent links, special files, conflicting
  paths, missing/non-executable `AppRun`, and a reserved-launcher collision.

`misc/regenerate_fixtures.py` generates the SquashFS image deterministically by
fixing timestamps, ownership, modes, compression settings, ordering, and
worker counts. DwarFS generation is not required on development hosts because
its tooling is substantially heavier than `mksquashfs`.

Integration tests must prove:

- `.AppImage` assets survive source filtering and rank as extractable assets;
- an extensionless direct AppImage is detected by content;
- install records only the package-name command, whose link remains inside the
  package and successfully runs an `AppRun` that requires `APPDIR`, `OWD`, and
  `ARGV0`;
- project scope leaves the source desktop entry inside the package without
  registering it; user and system scope create a managed desktop link in
  `$XDG_DATA_HOME/applications` and `/usr/share/applications`, respectively;
- desktop rewriting preserves metadata and action arguments, points `Exec` and
  `TryExec` at the renamed absolute command link, disables unsupported D-Bus
  activation, and resolves `Icon` to a safe file inside the installation;
- malformed, missing, and ambiguous desktop entries skip integration without
  losing the installed command, while desktop-link conflict, rollback, update,
  and ownership-aware uninstall behavior remain transactional;
- rename, force-conflict, update across SquashFS/DwarFS, rollback, `exec`, and
  uninstall preserve the existing transactional behavior;
- no test or production path executes the embedded AppImage runtime.

Before handoff, run the repository's normal gates:

```console
cargo fmt --check
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## Deliberate non-goals

- Type-1 ISO 9660 AppImages are not implemented because none occurred in the
  requested release histories. They are detected and rejected explicitly so a
  future ISO/Rock Ridge reader can be added without ambiguity.
- Mounting with FUSE and executing in place are not fallbacks.
- Installing icons into host icon-theme directories, updating shared desktop
  or MIME caches, AppImage self-update, portable home directories, and
  signature verification are separate features. Extraction preserves their
  files, and desktop integration uses an absolute in-package icon path rather
  than activating those other behaviors.
