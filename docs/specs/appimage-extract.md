# AppImage extraction design

## Status and decision

This document records an investigation performed on 2026-08-11 and proposes
AppImage support for `eget`. The implementation should support **type-2
AppImages with both SquashFS and DwarFS payloads**. Supporting only SquashFS
would install the older samples but fail for current Ghostty and mpv releases;
supporting only DwarFS would have the inverse problem.

`eget` must parse and extract the image itself. It must not execute the
downloaded runtime, call `--appimage-extract`, require FUSE, or shell out to
`unsquashfs`/`dwarfsextract` in normal operation. Filesystem reader crates may
provide decompression and filesystem parsing, while path validation and
materialization remain under `eget`'s control.

The installed command is named after `ResolvedPackage.app`, but it cannot be a
literal symlink to the extracted `AppRun` in all cases. The OpenOffice sample
requires `APPDIR` and `OWD`, and several launchers locate files relative to
their invocation path. The managed command therefore points to a small
`eget`-generated launcher which establishes the AppImage runtime environment
and then executes the root `AppRun`.

## Investigation

### Method

For each requested repository, the GitHub release list was paginated to find
the oldest and newest releases containing an AppImage. Representative x86-64
assets were downloaded in full. The AppImage magic, ELF layout, filesystem
offset, filesystem superblock, compression, and root `AppRun` were inspected
without using the AppImage runtime's extraction option. SquashFS images were
checked with a separately installed `unsquashfs`; DwarFS images were checked
with upstream `dwarfsextract`. These commands were investigation tools, not a
proposed runtime dependency.

“Newest” below means the newest published release with an AppImage, including
a prerelease or rolling release. Where useful, the newest stable release is
also shown. OpenOffice has only one release, so the same asset is both oldest
and newest.

### Downloaded assets

| Project | Position | Release and asset | Bytes | SHA-256 |
| --- | --- | --- | ---: | --- |
| [OpenOffice.AppImage](https://github.com/area-of-dev/OpenOffice.AppImage/releases) | oldest/newest | [`latest` / `OpenOffice.AppImage`](https://github.com/area-of-dev/OpenOffice.AppImage/releases/download/latest/OpenOffice.AppImage) | 167,748,800 | `221330cd07cf415e4b855e89c36647e91a15dca22dbe55c5182650a7f9f8b6bc` |
| [ghostty-appimage](https://github.com/pkgforge-dev/ghostty-appimage/releases) | oldest | [`v1.0.1` / `Ghostty-x86_64.AppImage`](https://github.com/pkgforge-dev/ghostty-appimage/releases/download/v1.0.1/Ghostty-x86_64.AppImage) | 20,587,248 | `b51871f17042edcda94cb4d9a7a148ab848be91fa7b04c3e3573b21c2584131c` |
| ghostty-appimage | newest stable | [`v1.3.1` / `Ghostty-1.3.1-x86_64.AppImage`](https://github.com/pkgforge-dev/ghostty-appimage/releases/download/v1.3.1/Ghostty-1.3.1-x86_64.AppImage) | 48,876,785 | `fde48d2b716afd1978766879bbf1aae30dd305e8ad86a1037a2614a14d82dc28` |
| ghostty-appimage | newest | [`tip` / `Ghostty-1.3.2-main-+d929e6a-x86_64.AppImage`](https://github.com/pkgforge-dev/ghostty-appimage/releases/download/tip/Ghostty-1.3.2-main-%2Bd929e6a-x86_64.AppImage) | 52,064,250 | `35ac0341494332a593d2ef91b443810948261283c09d2670178bb4576d5077af` |
| [mpv-AppImage](https://github.com/pkgforge-dev/mpv-AppImage/releases) | oldest | [`20241201-154313` / `mpv-v0.39.0-436-g744cd70640-anylinux-x86_64.AppImage`](https://github.com/pkgforge-dev/mpv-AppImage/releases/download/20241201-154313/mpv-v0.39.0-436-g744cd70640-anylinux-x86_64.AppImage) | 23,200,144 | `0b15c0420c0178a9b6ffc8056139e13d5787a669acef331398b4757308821b55` |
| mpv-AppImage | newest | [`nightly` / `mpv-v0.41.0-877-ge5486b96d-anylinux-x86_64.AppImage`](https://github.com/pkgforge-dev/mpv-AppImage/releases/download/nightly/mpv-v0.41.0-877-ge5486b96d-anylinux-x86_64.AppImage) | 52,290,281 | `7b1083319e859dc5a4e7cf0cda597f5c25be8a8639aac1dd2712521bce64af86` |
| [github/app](https://github.com/github/app/releases) | oldest | [`v0.2.0` / `GitHub-linux-x64.AppImage`](https://github.com/github/app/releases/download/v0.2.0/GitHub-linux-x64.AppImage) | 239,569,400 | `05cea410919f1a4f854d17f2c7cccc199b680c27163b7b0e3df6adb5af79d0f4` |
| github/app | newest | [`v1.1.6` / `GitHub-Copilot-linux-x64.AppImage`](https://github.com/github/app/releases/download/v1.1.6/GitHub-Copilot-linux-x64.AppImage) | 486,308,344 | `c2f539aa440623f85caa492e04f8e553d693b85f259cb92136928ab0b2fe6cd0` |
| [VLC-AppImage](https://github.com/ivan-hc/VLC-AppImage/releases) | oldest | [`3.0.16` / `VLC_media_player-GLIBC.2.27-x86_64.AppImage`](https://github.com/ivan-hc/VLC-appimage/releases/download/3.0.16/VLC_media_player-GLIBC.2.27-x86_64.AppImage) | 137,802,944 | `a42eb39de3fd5021379bf4ecc0eb185e4bbe98a453388005eaa8e71bbf3122bb` |
| VLC-AppImage | newest | [`continuous-git` / `VLC-media-player_GIT_4.0.0.r38520.g62f27a5-1-archimage5.0-x86_64.AppImage`](https://github.com/ivan-hc/VLC-appimage/releases/download/continuous-git/VLC-media-player_GIT_4.0.0.r38520.g62f27a5-1-archimage5.0-x86_64.AppImage) | 171,936,248 | `17ba5d34115644dbe44ebbd60b7a284d7b453a2e396148c0cb119f7a3d7be248` |

Ghostty, GitHub App, and mpv also publish aarch64 AppImages in recent
releases. Architecture does not change the envelope or extraction algorithm.

### Binary findings

Every downloaded file is a type-2 AppImage: it is an ELF executable and has
the bytes `41 49 02` (`AI`, type 2) at file offset 8. None of the samples is a
type-1 ISO 9660 AppImage.

| Sample | Filesystem offset | Filesystem | Compression | `AppRun` |
| --- | ---: | --- | --- | --- |
| OpenOffice `latest` | 189,632 | SquashFS 4.0 | gzip | executable shell script |
| Ghostty `v1.0.1` | 778,992 | SquashFS 4.0 | Zstandard | executable shell script |
| Ghostty `v1.3.1` | 1,483,248 | DwarFS 2.5 | Zstandard sections | executable shell script |
| Ghostty `tip` | 1,487,344 | DwarFS 2.5 | Zstandard sections | executable ELF, hard-linked to payload binaries |
| mpv oldest | 696,720 | SquashFS 4.0 | Zstandard | executable shell script |
| mpv newest | 1,454,544 | DwarFS 2.5 | Zstandard sections | executable shell script |
| github/app oldest | 944,632 | SquashFS 4.0 | Zstandard | executable Bash script |
| github/app newest | 944,632 | SquashFS 4.0 | Zstandard | executable Bash script |
| VLC oldest | 189,632 | SquashFS 4.0 | gzip | executable shell script |
| VLC newest | 944,632 | SquashFS 4.0 | Zstandard | executable shell script |

The endpoints alone establish that both filesystems are in active use. The
Ghostty change occurred between `v1.1.3` (SquashFS) and `v1.1.3+1` (DwarFS).
For mpv, `v0.40.0-37-g5870c95e8` is SquashFS and the next published build,
`v0.40.0-42-g36ea2354b`/`20250402-034245`, is DwarFS. This transition is a
publisher choice, not a new AppImage type: both payloads retain the type-2
magic and an ELF runtime capable of mounting the appended filesystem.

Searching the entire file for `hsqs` or `DWARFS` is not a sound offset
algorithm. Several runtime binaries contain earlier false matches (for
example, OpenOffice contains `hsqs` at 31,842 while its actual filesystem
starts at 189,632). The filesystem start must be derived from the ELF layout,
then confirmed by parsing the superblock at exactly that offset.

The root `AppRun` cannot be treated like an arbitrary discovered executable:

- It is the format-defined entry point and may be a script, ELF, symlink, or
  hard link.
- It may invoke other files in the extracted tree. Extracting only `AppRun` is
  insufficient.
- OpenOffice uses `APPDIR` and `OWD`; current Ghostty also uses `ARGV0` when
  choosing a payload command.
- A command symlink invoked from `$binDir` can cause an `AppRun` script that
  derives its directory from `$0` to look in `$binDir`, not in the AppDir.

The [AppImage specification](https://github.com/AppImage/AppImageSpec/blob/51c2a1465cfef1be7a159477ada8cc36a790e96c/draft.md)
defines the type-2 magic and root `AppRun`. The reference
[type-2 runtime](https://github.com/AppImage/type2-runtime/blob/75849dce7cc37e4319b633df1f116ca895c71a12/src/runtime/runtime.c)
computes the filesystem offset from the ELF section layout and establishes
`APPIMAGE`, `ARGV0`, `APPDIR`, and `OWD` before executing `AppRun`. The
alternative [uruntime](https://github.com/VHSgunzo/uruntime/tree/3c0f2b9fbe71fe0311884ec07817e805db8bd481)
explains why current publisher output may contain either SquashFS or DwarFS.

## Proposed implementation

### Dependencies

Use in-process, read-only readers:

- [`backhand`](https://docs.rs/backhand/0.25.1/backhand/) for SquashFS 4.0.
  It accepts a reader plus an explicit byte offset and supports gzip, xz, LZO,
  LZ4, and Zstandard when the corresponding features are enabled. Enable all
  standard SquashFS codecs so support is not restricted to the gzip and
  Zstandard samples.
- [`dwarfs`](https://docs.rs/dwarfs/0.2.1/dwarfs/) for DwarFS. Enable `zstd`,
  `lz4`, and `lzma`; construct it over a bounded offset reader. Version 0.2.1
  understands DwarFS filesystem versions 2.3 through 2.5, including the 2.5
  images observed here. Keep a real newest-publisher fixture because the crate
  only guarantees compatibility with the upstream versions named in its own
  documentation.

Do not invoke filesystem utilities from `src/archive.rs`. External utilities
make installation host-dependent and bypass the archive safety checks that
`eget` already owns.

### Source selection and naming

Add `.appimage` case-insensitively to all artifact-suffix logic in
`src/source.rs`:

1. It is a supported terminal release-asset suffix and receives the same
   archive score as other extractable package formats.
2. It is stripped during direct-URL application-name derivation.
3. `.AppImage.zsync`, signatures, and checksums remain rejected.

Asset names must still contain the normal Linux and architecture markers. On
macOS no AppImage candidate is selected; AppImage is a Linux format.

Detection must also be content-aware. Refactor `archive::extract` to inspect
the downloaded payload and return its detected format. A name ending in
`.AppImage` whose content is not a valid AppImage is an error, rather than a
plain executable. An extensionless direct download with valid `AI02` magic may
be recognized as an AppImage. Other extensionless files retain the existing
plain-executable behavior.

### Envelope parsing

Add `Format::AppImage` and an `src/appimage.rs` module. Parsing is read-only and
uses checked arithmetic throughout:

1. Read enough bytes for the ELF identification and AppImage magic. Require a
   supported little-endian 32- or 64-bit ELF executable with `AI02` at offset
   8. Validate the file length before every range read.
2. Parse the ELF header and section headers with the existing `object` crate.
   Compute the end of the ELF runtime as the maximum of:
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
   versions, malformed ELF layouts, overflows, overlaps, and truncated images.

A reusable `OffsetReader` should expose only `[payload_offset, file_length)`
to DwarFS and translate relative reads with checked addition. SquashFS can use
`backhand`'s native offset constructor. Neither reader may see bytes before the
payload as filesystem data.

### Safe materialization

Both backends feed a shared, format-neutral entry model containing a relative
path, entry kind, mode, optional link target, stable inode identity when
available, and a streaming file reader. Extraction follows the existing
security policy in `src/archive.rs`:

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

### Command mapping and launcher

Refactor `Prepared.binaries: Vec<PathBuf>` into explicit command mappings:

```text
PreparedCommand {
    name: String,          // recorded command and link name
    target: PathBuf,       // path below Prepared.root
}
```

For existing formats, both values initially come from the discovered binary
and current physical rename semantics stay unchanged. For an AppImage, create
exactly one mapping. Its initial `name` is `ResolvedPackage.app`; its target is
an `eget`-generated executable launcher stored at a reserved path in the
AppDir. Do not discover or expose the many other executables in the image.

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
$binDir/<package.app> -> <installation_dir>/.eget-appimage-launcher
                                      -> <installation_dir>/AppRun
```

For AppImages, `--rename FROM=TO` matches the initial logical package command
name and changes only that name; it must not rename `AppRun` or the generated
launcher. This exception should be added to `docs/specs/eget.md` when the
feature is implemented.

No database migration is required. The existing `binaries` table already
stores logical command names, while the managed symlink resolves beneath the
package installation directory as required by `exec` and uninstall.

### Desktop entry integration

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
valid desktop file, command installation may still succeed, but desktop
integration is skipped with a warning; the AppImage specification makes this
metadata recommended rather than mandatory.

The generated entry must preserve comments, localized values, unknown keys,
and application-action groups while making these controlled changes:

1. Rewrite the executable in the main `Exec=` value and every `[Desktop Action
   ...]` `Exec=` value to the final absolute managed command link
   `$binDir/<package.app>`. Parse and serialize the desktop-entry command-line
   grammar instead of using shell quoting, and preserve the publisher's
   remaining arguments and valid `%f`, `%F`, `%u`, `%U`, `%i`, `%c`, and `%k`
   field codes.
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

### Errors and candidate fallback

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

## Implementation sequence

1. Add AppImage suffix recognition and content-aware format detection.
2. Add the ELF envelope parser and unit tests for offset calculation and false
   magic matches.
3. Add the shared filesystem-entry validator/materializer, then the SquashFS
   and DwarFS adapters.
4. Refactor prepared binaries into command/target mappings and add the AppRun
   launcher.
5. Add scope-aware desktop entry rewriting and managed-link activation.
6. Update `docs/specs/eget.md`, fixture generation, and end-to-end tests.

## Tests and fixtures

Commit small deterministic type-2 fixtures under `tests/fixtures/archives`:

- one SquashFS 4.0 image and one DwarFS 2.5 image containing a root `AppRun`,
  a nested payload, executable modes, a safe relative symlink, and a hard link;
- AppRun variants for a script, ELF, and symlink;
- malformed envelopes: `AI01`, bad `AI02`, truncated section tables, overflowed
  ranges, a false filesystem magic inside the runtime, unknown payload magic,
  and unsupported DwarFS version;
- unsafe filesystem trees: absolute/parent links, special files, conflicting
  paths, missing/non-executable `AppRun`, and a reserved-launcher collision.

Extend `misc/regenerate_fixtures.py` so generation is deterministic. Fix all
timestamps, ownership, modes, compression settings, ordering, and worker
counts. If available DwarFS tooling cannot produce byte-identical output,
retain one reviewed canonical image as base64 in the generator, following the
existing encrypted-7z fixture pattern.

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
