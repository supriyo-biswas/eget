# AppImage extraction investigation

Status: implemented. The normative behavior is maintained in the
[AppImage specification](../specs/appimage.md).

The implementation was later placed behind the non-default Linux `extras`
feature so the filesystem readers do not increase the standard binary size.

## Status and decision

This document records the investigation performed on 2026-08-11 and the
resulting AppImage implementation. It supports **type-2
AppImages with both SquashFS and DwarFS payloads**. Supporting only SquashFS
would install the older samples but fail for current Ghostty and mpv releases;
supporting only DwarFS would have the inverse problem.

`eget` must parse and extract the image itself. It must not execute the
downloaded runtime, call `--appimage-extract`, require FUSE, or shell out to
`unsquashfs`/`dwarfsextract` in normal operation. Filesystem reader crates may
provide decompression and filesystem parsing, while path validation and
materialization remain under `eget`'s control.

The installed command cannot be a literal symlink to the extracted `AppRun` in
all cases. The OpenOffice sample requires `APPDIR` and `OWD`, and several
launchers locate files relative to their invocation path. The managed command
therefore points to a small `eget`-generated launcher which establishes the
AppImage runtime environment and then executes the root `AppRun`.

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

## Implemented sequence

1. Add AppImage suffix recognition and content-aware format detection.
2. Add the ELF envelope parser and unit tests for offset calculation and false
   magic matches.
3. Add the shared filesystem-entry validator/materializer, then the SquashFS
   and DwarFS adapters.
4. Refactor prepared binaries into command/target mappings and add the AppRun
   launcher.
5. Add scope-aware desktop entry rewriting and managed-link activation.
6. Update the specifications, fixture generation, and end-to-end tests.
