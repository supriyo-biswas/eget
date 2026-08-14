# Source resolution

Related specifications: [overview](eget.md), [identity and state](identity-and-state.md),
[installation](installation.md), and [package lifecycle](package-lifecycle.md).

## Authentication

For requests made against a forge's API (release listing, asset download), `eget` looks for a bearer/API token in the environment so users can avoid rate limits on unauthenticated requests and access private repos:

* **github.com:** `EGET_GITHUB_TOKEN`, falling back to the plain `GITHUB_TOKEN` environment variable if the former isn't set (mirroring the common convention used by other GitHub-aware CLIs, e.g. the `gh` CLI), sent as `Authorization: Bearer <token>`.
* **gitlab.com:** `EGET_GITLAB_TOKEN`, sent as `PRIVATE-TOKEN: <token>` (GitLab's conventional personal-access-token header).
* **gitea.com:** `EGET_GITEA_TOKEN`, sent as `Authorization: token <token>` (Gitea's conventional header).
* **Any other domain** (a custom GitLab/Gitea instance detected via probing): the token env var name is derived from the domain itself, and the header convention for the detected forge kind (`PRIVATE-TOKEN` for GitLab or `Authorization: token` for Gitea) is used.

**Direct URLs:** for `direct`-kind packages (and `--version-url` checks, see [Direct-URL version tracking](package-lifecycle.md#direct-url-version-tracking)), the same domain-derived `EGET_<DOMAINPART>_TOKEN` scheme is used, but there is no forge-specific convention to apply the value against — the environment variable is expected to already contain the *entire* header value as it should be sent, e.g. `EGET_MIN_IO_TOKEN=Bearer 12345`, and `eget` sends it verbatim as the `Authorization` header. Embedding credentials directly in a locref/URL passed to `eget install` (e.g. `https://user:token@host/...`) is not permitted; `eget` rejects such URLs and requires the token to be supplied via the environment instead.

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

## Domain probing

When a locref's domain is not `github.com`, `gitlab.com`, or `gitea.com`, `eget` must determine which forge (if any) is running at that domain before it can know how to query for releases. This uses the `source_probe_cache` table to avoid repeated network round-trips.

**Probe algorithm**, given a domain `D`:

1. Look up `D` in `source_probe_cache`. If a row exists and `checked_at` is within the last 12 hours, use its cached `kind` and skip network probing.
2. Otherwise, send `GET {origin}/api/v1/version`, preserving the locref's scheme, host, and explicit port.
3. If the response contains any header whose lowercase name starts with `x-gitlab-meta`, classify the origin as **GitLab**. The header is authoritative even on a non-success response.
4. Otherwise, if the response is successful and its body is a JSON object containing a string-valued `version` field, classify the origin as **Gitea**.
5. Otherwise, classify the origin as `unknown`; the locref is treated as a **direct URL**, no release API is consulted, and the URL is downloaded as-is.
6. Upsert `(domain, kind, checked_at=now())` into `source_probe_cache` regardless of outcome, so a domain confirmed not to be a forge is also cached.

A network failure, non-success response without a GitLab marker, oversized response, or malformed JSON does not fail installation by itself; it produces the `unknown`/direct result. No second probe endpoint is attempted.

## Package ID probe (resolving a locref)

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

## Application name derivation for direct URLs

For `direct`-kind packages (see step 9 above), there is no repository to ask "what's your name" — the normalized domain and app name must be derived heuristically from the URL:

**Normalized domain (and stored `owner`)** = the URL's hostname, with a common CDN/hosting subdomain label stripped from the front *only if* the hostname has at least two dots (i.e., is not already a bare two-label domain). The recognized labels are `www`, `download`, `downloads`, `dl`, `cache`, `cdn`, `release`, `releases`, `assets`, `static`, and `ftp`, each optionally followed by ASCII digits. An explicit non-default port is appended after normalization. For example `dl.min.io` → `min.io`, but `gitlab-docker-machine-downloads.s3.amazonaws.com` is *not* stripped because its first label is not an exact recognized label with an optional numeric suffix.

**App name** = derived from the last path segment:
1. If the URL has no path at all (or only `/`) — i.e. the domain itself serves the binary directly at its root, e.g. `eget https://test.example.com` — the app name is simply `default`, and there's no further stripping to do. This gives a package ID of `test.example.com/default`.
2. Otherwise, take the final `/`-delimited path segment.
3. Lowercase the segment and strip the longest recognized archive suffix: `.7z`, `.zip`, `.tar`, `.tar.gz`, `.tgz`, `.tar.bz2`, `.tbz`, `.tbz2`, `.tar.xz`, `.txz`, `.tar.zst`, `.tzst`, `.gz`, `.bz2`, `.xz`, or `.zst`. Format-specific suffix additions, including `.AppImage` in Linux extras builds, are defined by their format specification.
4. Retain the leading run consisting only of ASCII letters, digits, `-`, `_`, and `.`, then trim those delimiters from both ends. If the result does not begin with an ASCII letter, use `default`.
5. At the first delimiter followed by a removable artifact suffix, discard that delimiter and everything after it. A suffix is removable when it begins with a digit, begins with `v` followed by a digit, or begins with a recognized platform/packaging marker: `linux`, `win`, `windows`, `mac`, `macos`, `darwin`, `amd64`, `x86_64`, `x64`, `linux64`, `mac64`, `macos64`, `darwin64`, `arm64`, `aarch64`, `musl`, `glibc`, `gnu`, `static`, or `exe`. A marker may be followed by another `-`, `_`, or `.` component.
6. Trim trailing delimiters again. If no characters remain, use `default`.

Examples:
* `.../v0.16.2-gitlab.51/docker-machine-Linux-x86_64` → last segment `docker-machine-Linux-x86_64` → strip no extension → strip trailing `-Linux-x86_64` → `docker-machine`.
* `https://dl.min.io/aistor/mc/release/linux-amd64/mc` → last segment `mc` → nothing to strip → `mc` (owner: `min.io`, since `dl` is stripped).
* `https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip` → last segment `awscli-exe-linux-x86_64.zip` → strip `.zip` → the `linux` marker begins the removable suffix → `awscli-exe`.

## Forge suffix and tag classification

A suffix after a forge repository (`owner/repo:<suffix>`) or a tag obtained from a release URL is classified before release lookup:

1. A version-led suffix is an exact tag with no release selector. A value is version-led when it begins with an ASCII digit; begins with `v` followed by a digit; begins with `v` plus a version delimiter and contains a digit; or begins with `release`, `version`, `rel`, or `ver` followed by a digit or version delimiter. For example, `v1.2.3` and `2026-07-20` are exact tags.
2. Otherwise, if the suffix contains `-`, `_`, or `/` followed by a version-led remainder, the portion before that delimiter is the selector and the full suffix is an exact tag. For example, `gnu-sed-4.10` is exact tag `gnu-sed-4.10` with selector `gnu-sed`, and `kustomize/v5.8.1` is exact tag `kustomize/v5.8.1` with selector `kustomize`.
3. Otherwise, a valid selector by itself denotes a tracking selector. Selector components begin with an ASCII letter and contain only ASCII letters, digits, `.`, `-`, or `_`; `/` separates components. For example, `gnu-sed` tracks the newest matching tag.
4. Any remaining suffix is an exact tag without a selector.

Exact-tag requests are automatically pinned unless `--unpin`/`--no-pin` is supplied. A selector is appended to the package ID; an exact version without a selector is not. Consequently, installing tracking `owner/repo` and later installing exact tag `owner/repo:v1.2.3` addresses the same base package and leaves it pinned at the exact tag. For repository-named tags such as `jq-1.8.2`, both tracking and exact forms use `github.com/jqlang/jq:jq`; selecting the exact tag changes the existing record from tracking to pinned instead of creating a second package.

## Monorepo detection (forge-hosted packages)

Some forge repos publish releases for more than one distinct artifact family from the same repo — e.g. a repo named `static-builds` whose releases are tagged `gnu-sed-4.1`, `curl-8.2`, `wget-1.5`, etc., ordered by recency, with no single "latest" that makes sense across the whole repo. `eget` needs to detect this situation and require the user to disambiguate, rather than silently installing whatever happens to be the most recent tag regardless of family.

**Detection, when no release selector (`MonorepoPart`) was given** (i.e. plain `owner/repo`):

1. Select the forge's latest release for the requested channel using the provider-specific rules under [Installation](installation.md#installation) (for example, GitHub's `/releases/latest` for `stable`).
2. Derive an optional tag prefix using the same rules used for an explicit monorepo selector. For example, repo `jqlang/jq` with latest tag `jq-1.8.2` has prefix `jq`; repo `kustomize-sigs/kustomize` with latest tag `kustomize/v4.5.0` has prefix `kustomize`; version-led tag `v1.2.3` has no prefix.
3. If there is no derived prefix, proceed with the base package ID and leave `packages.release_selector` null.
4. If the derived prefix case-sensitively equals the repo name (the `app` segment), proceed with this release. Retain the prefix as the `MonorepoPart`: the package ID includes it (for example, `github.com/jqlang/jq:jq`) and `packages.release_selector` stores it for later updates. The matching prefix means the unqualified request is unambiguous; it does not mean the selector should be discarded.
5. If a derived prefix does **not** match (e.g. repo `static-builds` with latest tag `gnu-sed-4.1`), `eget` refuses to guess and fails with an error telling the user to re-run with an explicit monorepo selector, e.g. `eget install supriyo-biswas/static-builds:gnu-sed`.

**Once a release selector (`MonorepoPart`) is known** (whether given explicitly, derived from a matching repository-named tag prefix, or loaded from `release_selector` on a prior install), `eget` needs to find the newest release whose tag matches `<selector><boundary-or-end>`. The exact lookup mechanism varies by forge, since none of them expose a "give me the latest *release* whose tag has this prefix" endpoint directly:

* **GitLab**: list releases newest-first and scan them client-side for the first tag satisfying the selector rule.
* **GitHub**: neither the releases nor tags listing endpoint accepts a name/prefix filter. List releases newest-first and scan them client-side for the first tag satisfying the selector rule.
* **Gitea**: pass `tag_filter=<selector>*` to the release-list endpoint so versions that support the parameter can reduce the result set server-side. Older versions ignore the unknown parameter and return the unfiltered list. In either case, scan releases newest-first and apply the selector rule client-side because Gitea's filter is case-insensitive and does not enforce the selector boundary. When the requested channel is `prerelease`, also pass Gitea's `pre-release=true` release-list filter so non-prereleases do not consume the client-side scan.

Every paginated release or tag scan requests the forge's maximum supported page size and fetches at most **5 pages**. If no match is found within those 5 pages, installation/update fails rather than silently choosing a tag from another artifact family or scanning an unbounded history.

This same channel-aware lookup (fetch-latest-and-compare, or forge-appropriate prefix scan once a selector is known) is used both for the initial `install` and for subsequent `update` runs.

## Asset selection

Asset names are matched case-insensitively. Reject signatures, checksums, and
source archives. A candidate must contain a recognized host OS marker and
architecture marker at a non-alphanumeric boundary, and must end in a
supported archive suffix, a recognized platform suffix, the release tag, or
no extension. When the Linux `extras` feature is enabled, format-specific
exceptions for AppImage assets are defined in the
[AppImage specification](appimage.md#source-selection-and-naming). Without
that feature, AppImage names receive no special suffix or platform handling.
A raw
platform suffix may express the OS and architecture in either order, with one
or more non-alphanumeric separators between them (for example,
`linux-amd64`, `linux.x64`, `linux_x86_64`, or `amd64_linux`); the pair must
end the filename. Rank the remaining candidates as follows:

* Add 10 points for a supported archive suffix.
* On Linux, add 5 points for a `static` marker.
* When the host libc is known, add 20 points for the matching libc (`glibc`/`gnu` or `musl`) and subtract 1 point for the incompatible libc. An unmarked build therefore ranks above an explicitly incompatible one.
* On Linux, detect a desktop toolkit from `XDG_CURRENT_DESKTOP`, falling back to `DESKTOP_SESSION` when the first value is absent or unrecognized. Matching is case-insensitive and recognizes colon-separated identifiers. GNOME/Ubuntu, Unity, Cinnamon, MATE, Xfce, LXDE, Budgie, and Pantheon select GTK; KDE/Plasma and LXQt select Qt. Unknown or conflicting identifiers select neither.
* For asset-name words matched at non-alphanumeric boundaries, add 1 point to the detected `gtk` or `qt` marker and subtract 1 point from the other marker. With no detected toolkit, subtract 1 point from either marker. Thus otherwise-equivalent candidates rank as matching toolkit, unmarked, then non-matching toolkit, while every variant remains eligible as a fallback.
* Use the package's persisted `asset_preferences` during updates and reinstalls. For a migrated package whose preference is still null, detect it for the next attempted installation and persist it only after that installation succeeds.
* Linux-specific libc and static markers do not affect macOS ranking.

Candidates are attempted in descending score order until one yields at least
one compatible executable. If all candidates fail, installation reports the
failure associated with each candidate.
