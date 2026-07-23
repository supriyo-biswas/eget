use crate::{
    compat::{self, HostArch, HostOs},
    db::Database,
    model::ProbeKind,
    policy::Channel,
};
use anyhow::{Context, Result, bail};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use regex::Regex;
use reqwest::blocking::{Client, Response};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, LINK, LOCATION,
};
use serde::Deserialize;
use serde_json::Value;
use std::cmp::Reverse;
use std::error::Error;
use std::fmt;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

const MAX_API_BODY: u64 = 2 * 1024 * 1024;
const MAX_PROBE_BODY: u64 = 64 * 1024;
const MAX_REDIRECTS: usize = 10;
const RELEASE_PAGE_SIZE: usize = 100;
const MAX_RELEASE_PAGES: usize = 5;
const PROBE_CACHE_SECONDS: i64 = 12 * 60 * 60;

static DIRECT_URL_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[^0-9][0-9]+\.[0-9]+\.[0-9]+(?:$|[./_-])")
        .expect("direct URL version regex is valid")
});

pub use crate::model::SourceKind;

#[derive(Clone, Debug)]
pub struct AssetCandidate {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct ResolvedPackage {
    pub id: String,
    pub owner: String,
    pub app: String,
    pub kind: SourceKind,
    pub source: String,
    pub tag: Option<String>,
    pub automatic_pin: bool,
    pub pinned: bool,
    pub channel: Channel,
    pub release_selector: Option<String>,
    pub forge_origin: Option<String>,
    pub candidates: Vec<AssetCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageIdentityHint {
    pub ids: Vec<String>,
    pub exact: bool,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct GitlabRelease {
    tag_name: String,
    assets: GitlabAssets,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReleaseRequest {
    Latest,
    Prefix(String),
    Exact {
        tag: String,
        selector: Option<String>,
    },
}

#[derive(Debug)]
pub struct SelectorNotFound {
    pub selector: String,
}

impl fmt::Display for SelectorNotFound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "no release found for package {} within the first five release pages",
            self.selector
        )
    }
}

impl Error for SelectorNotFound {}

#[derive(Debug)]
pub struct MonorepoLatest {
    pub tag: String,
    pub selector: String,
    pub source: String,
}

impl fmt::Display for MonorepoLatest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "latest release tag {} looks like a monorepo release; install a tool explicitly, for example: eget install {}:{}",
            self.tag, self.source, self.selector
        )
    }
}

impl Error for MonorepoLatest {}

#[derive(Deserialize)]
struct GitlabAssets {
    links: Vec<GitlabLink>,
}

#[derive(Deserialize)]
struct GitlabLink {
    name: String,
    url: String,
    direct_asset_url: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Libc {
    Glibc,
    Musl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Platform {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Linux { arch: HostArch, libc: Option<Libc> },
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Macos { arch: HostArch },
}

impl Platform {
    fn os(self) -> HostOs {
        match self {
            Self::Linux { .. } => HostOs::Linux,
            Self::Macos { .. } => HostOs::Macos,
        }
    }

    fn arch(self) -> HostArch {
        match self {
            Self::Linux { arch, .. } | Self::Macos { arch } => arch,
        }
    }
}

#[derive(Debug)]
enum ForgeInput {
    Repository {
        project: Vec<String>,
        tag: Option<String>,
    },
    Direct {
        project: Vec<String>,
        tag: String,
        name: String,
    },
}

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("eget/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

pub fn resolve(client: &Client, input: &str) -> Result<ResolvedPackage> {
    resolve_with_preferences(client, input, None, Channel::Stable, None)
}

pub fn resolve_with_hint(
    client: &Client,
    input: &str,
    hint: Option<SourceKind>,
) -> Result<ResolvedPackage> {
    resolve_with_preferences(client, input, hint, Channel::Stable, None)
}

pub fn resolve_with_preferences(
    client: &Client,
    input: &str,
    hint: Option<SourceKind>,
    channel: Channel,
    release_selector: Option<&str>,
) -> Result<ResolvedPackage> {
    if matches!(hint, None | Some(SourceKind::Github))
        && let Some((owner, repo, tag)) = parse_repo(input)
    {
        let source = format!("{owner}/{repo}");
        let request = release_selector.map_or_else(
            || release_request(tag),
            |selector| ReleaseRequest::Prefix(selector.to_owned()),
        );
        return resolve_github(client, owner, repo, request, &source, channel);
    }

    let url = normalized_url(input)?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("credentials in package URLs are not allowed; use an EGET_*_TOKEN variable")
    }
    let host = url.host_str().context("URL has no host")?.to_owned();
    let kind = hint.unwrap_or_else(|| known_forge(&host).unwrap_or(SourceKind::Direct));
    let kind = if hint.is_none() && kind == SourceKind::Direct {
        probe_forge(client, &url).unwrap_or(SourceKind::Direct)
    } else {
        kind
    };

    if kind == SourceKind::Github {
        return resolve_github_url(client, &url, input, channel, release_selector);
    }
    if matches!(kind, SourceKind::Gitea | SourceKind::Gitlab) {
        let parsed = parse_forge_url(&url, kind)
            .with_context(|| format!("invalid {} URL: {input}", kind.as_str()))?;
        return resolve_forge(client, &url, parsed, kind, channel, release_selector);
    }

    let app = direct_app(&url);
    let mut owner = normalized_owner(&host);
    if let Some(port) = url.port() {
        owner.push_str(&format!(":{port}"));
    }
    let pinned = direct_url_has_version(&url);
    let name = url_asset_name(&url, &app);
    Ok(ResolvedPackage {
        id: format!("{owner}/{app}"),
        owner,
        app,
        kind: SourceKind::Direct,
        source: redact(&url),
        tag: None,
        automatic_pin: pinned,
        pinned,
        channel,
        release_selector: None,
        forge_origin: None,
        candidates: vec![AssetCandidate {
            name,
            url: url.to_string(),
        }],
    })
}

pub fn resolve_with_store(
    client: &Client,
    database: &Database,
    input: &str,
    channel: Channel,
    release_selector: Option<&str>,
) -> Result<ResolvedPackage> {
    if parse_repo(input).is_some() {
        return resolve_with_preferences(client, input, None, channel, release_selector);
    }
    let url = normalized_url(input)?;
    let host = url.host_str().context("URL has no host")?;
    let probe_domain = package_source(&url)?;
    let hint = if let Some(kind) = known_forge(host) {
        kind
    } else {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let cached = database
            .probe(&probe_domain)?
            .filter(|(_, checked_at)| now - checked_at < PROBE_CACHE_SECONDS)
            .map(|(kind, _)| kind);
        let probe = match cached {
            Some(kind) => kind,
            None => {
                let detected = match probe_forge_uncached(client, &origin_url(&url)?) {
                    Ok(Some(SourceKind::Gitea)) => ProbeKind::Gitea,
                    Ok(Some(SourceKind::Gitlab)) => ProbeKind::Gitlab,
                    Ok(Some(_)) | Ok(None) | Err(_) => ProbeKind::Unknown,
                };
                database.put_probe(&probe_domain, detected, now)?;
                detected
            }
        };
        match probe {
            ProbeKind::Gitea => SourceKind::Gitea,
            ProbeKind::Gitlab => SourceKind::Gitlab,
            ProbeKind::Unknown => SourceKind::Direct,
        }
    };
    resolve_with_preferences(client, input, Some(hint), channel, release_selector)
}

pub fn package_identity_hint(
    input: &str,
    kind_hint: Option<SourceKind>,
) -> Result<PackageIdentityHint> {
    if matches!(kind_hint, None | Some(SourceKind::Github))
        && let Some((owner, repo, tag)) = parse_repo(input)
    {
        let project = [owner.to_owned(), repo.to_owned()];
        let (id, owner, app) = forge_identity("github.com", &project)?;
        return identity_hint_for_request(id, owner, app, release_request(tag), repo);
    }

    let url = normalized_url(input)?;
    let host = url.host_str().context("URL has no host")?;
    let kind = kind_hint
        .or_else(|| known_forge(host))
        .unwrap_or(SourceKind::Direct);
    if kind == SourceKind::Direct {
        let app = direct_app(&url);
        let mut owner = normalized_owner(host);
        if let Some(port) = url.port() {
            owner.push_str(&format!(":{port}"));
        }
        return Ok(PackageIdentityHint {
            ids: vec![format!("{owner}/{app}")],
            exact: false,
        });
    }

    if kind == SourceKind::Github {
        let parts = decoded_path_segments(&url)?;
        if parts.len() >= 6
            && parts[2..4] == ["releases", "download"]
            && !parts[..2].iter().any(|part| part.contains(':'))
        {
            let (id, owner, app) = forge_identity(host, &parts[..2])?;
            let selector = monorepo_selector(&parts[4]).filter(|value| value != &parts[4]);
            let (id, _, _) = selected_identity(id, owner, app, selector.as_deref());
            return Ok(PackageIdentityHint {
                ids: vec![id],
                exact: true,
            });
        }
        let (project, tag) = if parts.len() == 5
            && parts[2..4] == ["releases", "tag"]
            && !parts[..2].iter().any(|part| part.contains(':'))
        {
            (parts[..2].to_vec(), Some(parts[4].clone()))
        } else if let Some((project, tag)) = tagged_repository_path(&url)? {
            (project, Some(tag))
        } else if parts.len() == 2 {
            (parts, None)
        } else {
            bail!("invalid GitHub URL: {input}")
        };
        if project.len() != 2 {
            bail!("invalid GitHub URL: {input}")
        }
        let (id, owner, app) = forge_identity(host, &project)?;
        return identity_hint_for_request(
            id,
            owner,
            app,
            release_request(tag.as_deref()),
            project.last().unwrap(),
        );
    }

    let parsed = parse_forge_url(&url, kind)
        .with_context(|| format!("invalid {} URL: {input}", kind.as_str()))?;
    let (project, request) = match parsed {
        ForgeInput::Repository { project, tag } => {
            let request = release_request(tag.as_deref());
            (project, request)
        }
        ForgeInput::Direct { project, tag, .. } => {
            let selector = monorepo_selector(&tag).filter(|value| value != &tag);
            (project, ReleaseRequest::Exact { tag, selector })
        }
    };
    let (id, owner, app) = forge_identity(host, &project)?;
    identity_hint_for_request(
        id,
        owner,
        app,
        request,
        project.last().context("repository path is empty")?,
    )
}

fn identity_hint_for_request(
    id: String,
    owner: String,
    app: String,
    request: ReleaseRequest,
    repository: &str,
) -> Result<PackageIdentityHint> {
    let exact = matches!(request, ReleaseRequest::Exact { .. });
    let selector = match request {
        ReleaseRequest::Latest => None,
        ReleaseRequest::Prefix(selector) => Some(selector),
        ReleaseRequest::Exact { selector, .. } => selector,
    };
    let (selected, _, _) = selected_identity(id.clone(), owner, app, selector.as_deref());
    let mut ids = vec![selected];
    if !exact && selector.is_none() {
        ids.push(format!("{id}:{}", repository.to_ascii_lowercase()));
    }
    ids.dedup();
    Ok(PackageIdentityHint { ids, exact })
}

fn normalized_url(input: &str) -> Result<Url> {
    let text = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{}", input.trim_end_matches('/'))
    };
    let mut url = Url::parse(&text).context("invalid package URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("invalid URL scheme: {}", url.scheme());
    }
    let host = url
        .host_str()
        .context("URL has no host")?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    url.set_host(Some(&host))?;
    let mut collapsed_path = String::with_capacity(url.path().len());
    for character in url.path().chars() {
        if character != '/' || !collapsed_path.ends_with('/') {
            collapsed_path.push(character);
        }
    }
    url.set_path(&collapsed_path);
    Ok(url)
}

fn known_forge(host: &str) -> Option<SourceKind> {
    match host {
        "github.com" => Some(SourceKind::Github),
        "gitea.com" => Some(SourceKind::Gitea),
        "gitlab.com" => Some(SourceKind::Gitlab),
        _ => None,
    }
}

fn probe_forge(client: &Client, url: &Url) -> Option<SourceKind> {
    probe_forge_uncached(client, &origin_url(url).ok()?)
        .ok()
        .flatten()
}

fn probe_forge_uncached(client: &Client, origin: &Url) -> Result<Option<SourceKind>> {
    let endpoint = origin.join("api/v1/version")?;
    let Ok(mut response) = client.get(endpoint).send() else {
        return Ok(None);
    };
    if response
        .headers()
        .keys()
        .any(|name| name.as_str().starts_with("x-gitlab-meta"))
    {
        return Ok(Some(SourceKind::Gitlab));
    }
    if !response.status().is_success() {
        return Ok(None);
    }
    let bytes = read_limited(&mut response, MAX_PROBE_BODY, "forge probe")?;
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok(value
        .as_object()
        .and_then(|object| object.get("version"))
        .and_then(Value::as_str)
        .map(|_| SourceKind::Gitea))
}

fn parse_forge_url(url: &Url, kind: SourceKind) -> Result<ForgeInput> {
    let parts = decoded_path_segments(url)?;
    match kind {
        SourceKind::Gitea => {
            if parts.len() == 5
                && parts[2..4] == ["releases", "tag"]
                && !parts[..2].iter().any(|part| part.contains(':'))
            {
                return Ok(ForgeInput::Repository {
                    project: parts[..2].to_vec(),
                    tag: Some(parts[4].clone()),
                });
            }
            if parts.len() >= 6
                && parts[2..4] == ["releases", "download"]
                && !parts[..2].iter().any(|part| part.contains(':'))
            {
                return Ok(ForgeInput::Direct {
                    project: parts[..2].to_vec(),
                    tag: parts[4].clone(),
                    name: parts[5..].join("/"),
                });
            }
            if let Some((project, tag)) = tagged_repository_path(url)? {
                if project.len() == 2 {
                    return Ok(ForgeInput::Repository {
                        project,
                        tag: Some(tag),
                    });
                }
                bail!("expected a root-mounted Gitea repository or release URL")
            }
            if parts.len() == 2 {
                return Ok(ForgeInput::Repository {
                    project: parts,
                    tag: None,
                });
            }
            bail!("expected a root-mounted Gitea repository or release URL")
        }
        SourceKind::Gitlab => {
            if let Some(marker) = parts
                .iter()
                .position(|part| part == "-")
                .filter(|marker| !parts[..*marker].iter().any(|part| part.contains(':')))
            {
                if marker == 0 || parts.get(marker + 1).map(String::as_str) != Some("releases") {
                    bail!("invalid GitLab release URL")
                }
                let project = parts[..marker].to_vec();
                if parts.len() == marker + 3 {
                    return Ok(ForgeInput::Repository {
                        project,
                        tag: Some(parts[marker + 2].clone()),
                    });
                }
                if parts.len() >= marker + 5 && parts[marker + 3] == "downloads" {
                    return Ok(ForgeInput::Direct {
                        project,
                        tag: parts[marker + 2].clone(),
                        name: parts[marker + 4..].join("/"),
                    });
                }
                bail!("invalid GitLab release URL")
            }
            if let Some((project, tag)) = tagged_repository_path(url)? {
                return Ok(ForgeInput::Repository {
                    project,
                    tag: Some(tag),
                });
            }
            if parts.is_empty() {
                bail!("GitLab repository path is empty")
            }
            Ok(ForgeInput::Repository {
                project: parts,
                tag: None,
            })
        }
        _ => bail!("not a Gitea or GitLab URL"),
    }
}

fn tagged_repository_path(url: &Url) -> Result<Option<(Vec<String>, String)>> {
    let path = percent_decode_str(url.path())
        .decode_utf8()
        .context("URL path is not valid UTF-8")?;
    let Some((project, tag)) = path.split_once(':') else {
        return Ok(None);
    };
    let project = project
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if project.is_empty() || tag.is_empty() {
        bail!("repository and tag must not be empty")
    }
    Ok(Some((project, tag.to_owned())))
}

fn release_request(tag: Option<&str>) -> ReleaseRequest {
    let Some(tag) = tag else {
        return ReleaseRequest::Latest;
    };
    if version_led(tag) {
        return ReleaseRequest::Exact {
            tag: tag.to_owned(),
            selector: None,
        };
    }
    if let Some(selector) = monorepo_selector(tag) {
        if selector == tag {
            ReleaseRequest::Prefix(selector)
        } else {
            ReleaseRequest::Exact {
                tag: tag.to_owned(),
                selector: Some(selector),
            }
        }
    } else {
        ReleaseRequest::Exact {
            tag: tag.to_owned(),
            selector: None,
        }
    }
}

fn monorepo_selector(tag: &str) -> Option<String> {
    if version_led(tag) {
        return None;
    }
    for (index, character) in tag.char_indices() {
        if matches!(character, '-' | '_' | '/') {
            let tail = &tag[index + character.len_utf8()..];
            let selector = &tag[..index];
            if version_led(tail) && valid_selector(selector) {
                return Some(selector.to_owned());
            }
        }
    }
    valid_selector(tag).then(|| tag.to_owned())
}

fn version_led(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if bytes.first().is_some_and(u8::is_ascii_digit)
        || (bytes.first().is_some_and(u8::is_ascii_alphabetic)
            && (bytes.get(1).is_some_and(u8::is_ascii_digit)
                || bytes.get(1).is_some_and(|byte| {
                    matches!(byte, b'-' | b'_' | b'/' | b'.')
                        && bytes[2..].iter().any(u8::is_ascii_digit)
                })))
    {
        return true;
    }
    ["release", "version", "rel", "ver"].iter().any(|marker| {
        lower.strip_prefix(marker).is_some_and(|tail| {
            tail.as_bytes().first().is_some_and(|byte| {
                byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'/' | b'.')
            })
        })
    })
}

fn valid_selector(selector: &str) -> bool {
    !selector.is_empty()
        && selector.split('/').all(|component| {
            component.starts_with(|character: char| character.is_ascii_alphabetic())
                && component
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
        })
}

fn selector_matches(tag: &str, selector: &str) -> bool {
    tag == selector
        || tag.strip_prefix(selector).is_some_and(|tail| {
            tail.as_bytes()
                .first()
                .is_some_and(|byte| !byte.is_ascii_alphanumeric())
        })
}

fn selected_identity(
    id: String,
    owner: String,
    app: String,
    selector: Option<&str>,
) -> (String, String, String) {
    let Some(selector) = selector else {
        return (id, owner, app);
    };
    (format!("{id}:{selector}"), owner, app)
}

fn selector_for_latest(tag: &str, repository: &str, source: &str) -> Result<Option<String>> {
    let Some(selector) = monorepo_selector(tag) else {
        return Ok(None);
    };
    if selector == repository {
        return Ok(Some(selector));
    }
    Err(MonorepoLatest {
        tag: tag.to_owned(),
        selector,
        source: source.to_owned(),
    }
    .into())
}

fn resolve_forge(
    client: &Client,
    input_url: &Url,
    parsed: ForgeInput,
    kind: SourceKind,
    channel: Channel,
    release_selector: Option<&str>,
) -> Result<ResolvedPackage> {
    if kind == SourceKind::Gitlab && channel == Channel::Prerelease {
        bail!("GitLab does not support the prerelease channel")
    }
    let origin = origin_url(input_url)?;
    let (project, request, direct) = match parsed {
        ForgeInput::Repository { project, tag } => {
            let request = release_selector.map_or_else(
                || release_request(tag.as_deref()),
                |selector| ReleaseRequest::Prefix(selector.to_owned()),
            );
            (project, request, None)
        }
        ForgeInput::Direct { project, tag, name } => {
            let selector = monorepo_selector(&tag).filter(|selector| selector != &tag);
            (project, ReleaseRequest::Exact { tag, selector }, Some(name))
        }
    };
    let source = repository_source(&origin, &project)?;
    let (base_id, owner, app) = forge_identity(&package_source(input_url)?, &project)?;
    if let Some(name) = direct {
        let ReleaseRequest::Exact { tag, selector } = request else {
            unreachable!()
        };
        let (id, owner, app) = selected_identity(base_id, owner, app, selector.as_deref());
        return Ok(ResolvedPackage {
            id,
            owner,
            app,
            kind,
            source,
            tag: Some(tag),
            automatic_pin: true,
            pinned: true,
            channel,
            release_selector: selector,
            forge_origin: Some(origin.to_string()),
            candidates: vec![AssetCandidate {
                name,
                url: input_url.to_string(),
            }],
        });
    }

    let pinned = matches!(request, ReleaseRequest::Exact { .. });
    let mut selector = match &request {
        ReleaseRequest::Prefix(selector) => Some(selector.clone()),
        ReleaseRequest::Exact { selector, .. } => selector.clone(),
        ReleaseRequest::Latest => None,
    };
    let (tag, mut candidates) = fetch_forge_release(
        client,
        &origin,
        kind,
        &project,
        &request,
        channel.allows_prereleases(),
    )?;
    if matches!(request, ReleaseRequest::Latest) {
        selector = selector_for_latest(&tag, project.last().unwrap(), &source)?;
    }
    filter_and_sort_assets(&mut candidates, &tag)?;
    if candidates.is_empty() {
        bail!(
            "no supported release assets found for {}",
            project.join("/")
        );
    }
    let (id, owner, app) = selected_identity(base_id, owner, app, selector.as_deref());
    Ok(ResolvedPackage {
        id,
        owner,
        app,
        kind,
        source,
        tag: Some(tag),
        automatic_pin: pinned,
        pinned,
        channel,
        release_selector: selector,
        forge_origin: Some(origin.to_string()),
        candidates,
    })
}

fn gitlab_asset_candidate(link: GitlabLink, forge_origin: &Url) -> AssetCandidate {
    let external_target =
        Url::parse(&link.url).is_ok_and(|target| !same_origin(forge_origin, &target));
    AssetCandidate {
        name: link.name,
        url: if external_target {
            link.url
        } else {
            link.direct_asset_url.unwrap_or(link.url)
        },
    }
}

fn repository_source(origin: &Url, project: &[String]) -> Result<String> {
    let mut url = origin.clone();
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid forge origin"))?;
        segments.pop_if_empty();
        segments.extend(project.iter().map(String::as_str));
    }
    Ok(url.to_string())
}

fn fetch_forge_release(
    client: &Client,
    origin: &Url,
    kind: SourceKind,
    project: &[String],
    request: &ReleaseRequest,
    allow_prerelease: bool,
) -> Result<(String, Vec<AssetCandidate>)> {
    match kind {
        SourceKind::Gitea => {
            let release = match request {
                ReleaseRequest::Exact { tag, .. } => api_json(
                    client,
                    release_endpoint(origin, kind, project, Some(tag))?,
                    kind,
                )?,
                ReleaseRequest::Latest if !allow_prerelease => {
                    api_json(client, release_endpoint(origin, kind, project, None)?, kind)?
                }
                ReleaseRequest::Latest => {
                    find_github_style_release(client, origin, kind, project, None, true)?
                }
                ReleaseRequest::Prefix(selector) => find_github_style_release(
                    client,
                    origin,
                    kind,
                    project,
                    Some(selector),
                    allow_prerelease,
                )?,
            };
            Ok(github_release_parts(release))
        }
        SourceKind::Gitlab => {
            let release = match request {
                ReleaseRequest::Exact { tag, .. } => api_json(
                    client,
                    release_endpoint(origin, kind, project, Some(tag))?,
                    kind,
                )?,
                ReleaseRequest::Latest => {
                    api_json(client, release_endpoint(origin, kind, project, None)?, kind)?
                }
                ReleaseRequest::Prefix(selector) => {
                    find_gitlab_release(client, origin, project, selector)?
                }
            };
            Ok(gitlab_release_parts(release, origin))
        }
        _ => unreachable!(),
    }
}

fn github_release_parts(release: GithubRelease) -> (String, Vec<AssetCandidate>) {
    (
        release.tag_name,
        release
            .assets
            .into_iter()
            .map(|asset| AssetCandidate {
                name: asset.name,
                url: asset.browser_download_url,
            })
            .collect(),
    )
}

fn gitlab_release_parts(release: GitlabRelease, origin: &Url) -> (String, Vec<AssetCandidate>) {
    (
        release.tag_name,
        release
            .assets
            .links
            .into_iter()
            .map(|link| gitlab_asset_candidate(link, origin))
            .collect(),
    )
}

fn find_github_style_release(
    client: &Client,
    origin: &Url,
    kind: SourceKind,
    project: &[String],
    selector: Option<&str>,
    allow_prerelease: bool,
) -> Result<GithubRelease> {
    for page in 1..=MAX_RELEASE_PAGES {
        let endpoint = release_list_endpoint(origin, kind, project, page)?;
        let (releases, has_next): (Vec<GithubRelease>, _) = api_json_page(client, endpoint, kind)?;
        if let Some(release) = releases.into_iter().find(|release| {
            !release.draft
                && (allow_prerelease || !release.prerelease)
                && selector.is_none_or(|selector| selector_matches(&release.tag_name, selector))
        }) {
            return Ok(release);
        }
        if !has_next {
            break;
        }
    }
    match selector {
        Some(selector) => Err(SelectorNotFound {
            selector: selector.to_owned(),
        }
        .into()),
        None => bail!("no eligible {} release found", kind.as_str()),
    }
}

fn find_gitlab_release(
    client: &Client,
    origin: &Url,
    project: &[String],
    selector: &str,
) -> Result<GitlabRelease> {
    for page in 1..=MAX_RELEASE_PAGES {
        let endpoint = release_list_endpoint(origin, SourceKind::Gitlab, project, page)?;
        let (releases, has_next): (Vec<GitlabRelease>, _) =
            api_json_page(client, endpoint, SourceKind::Gitlab)?;
        if let Some(release) = releases
            .into_iter()
            .find(|release| selector_matches(&release.tag_name, selector))
        {
            return Ok(release);
        }
        if !has_next {
            break;
        }
    }
    Err(SelectorNotFound {
        selector: selector.to_owned(),
    }
    .into())
}

fn release_list_endpoint(
    origin: &Url,
    kind: SourceKind,
    project: &[String],
    page: usize,
) -> Result<Url> {
    let mut url = match kind {
        SourceKind::Github => {
            let [owner, repo] = project else {
                bail!("invalid GitHub project path")
            };
            let mut url = origin.join("repos/")?;
            url.path_segments_mut()
                .map_err(|_| anyhow::anyhow!("invalid GitHub API origin"))?
                .pop_if_empty()
                .extend([owner.as_str(), repo.as_str(), "releases"]);
            url
        }
        SourceKind::Gitea => {
            let mut url = origin.join("api/v1/repos/")?;
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| anyhow::anyhow!("invalid Gitea origin"))?;
                segments.pop_if_empty();
                segments.extend(project.iter().map(String::as_str));
                segments.push("releases");
            }
            url
        }
        SourceKind::Gitlab => {
            let project_path = project.join("/");
            let encoded = utf8_percent_encode(&project_path, NON_ALPHANUMERIC);
            origin.join(&format!("api/v4/projects/{encoded}/releases"))?
        }
        SourceKind::Direct => bail!("direct URLs have no release list"),
    };
    let page_size = if kind == SourceKind::Gitea {
        "limit"
    } else {
        "per_page"
    };
    url.query_pairs_mut()
        .append_pair(page_size, &RELEASE_PAGE_SIZE.to_string())
        .append_pair("page", &page.to_string());
    Ok(url)
}

fn release_endpoint(
    origin: &Url,
    kind: SourceKind,
    project: &[String],
    tag: Option<&str>,
) -> Result<Url> {
    match kind {
        SourceKind::Github => {
            let [owner, repo] = project else {
                bail!("invalid GitHub project path")
            };
            let mut url = origin.join("repos/")?;
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("invalid GitHub API origin"))?;
            segments
                .pop_if_empty()
                .extend([owner.as_str(), repo.as_str(), "releases"]);
            if let Some(tag) = tag {
                segments.push("tags");
                drop(segments);
                let tag = utf8_percent_encode(tag, NON_ALPHANUMERIC);
                return Url::parse(&format!("{}/{tag}", url.as_str().trim_end_matches('/')))
                    .context("invalid GitHub release endpoint");
            } else {
                segments.push("latest");
            }
            drop(segments);
            Ok(url)
        }
        SourceKind::Gitea => {
            let mut url = origin.join("api/v1/repos/")?;
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| anyhow::anyhow!("invalid Gitea origin"))?;
                segments.pop_if_empty();
                segments.extend(project.iter().map(String::as_str));
                segments.push("releases");
                if let Some(tag) = tag {
                    segments.push("tags").push(tag);
                } else {
                    segments.push("latest");
                }
            }
            Ok(url)
        }
        SourceKind::Gitlab => {
            let project_path = project.join("/");
            let encoded_project = utf8_percent_encode(&project_path, NON_ALPHANUMERIC);
            let tail = tag.map_or_else(
                || "permalink/latest".to_owned(),
                |tag| utf8_percent_encode(tag, NON_ALPHANUMERIC).to_string(),
            );
            Ok(origin.join(&format!(
                "api/v4/projects/{encoded_project}/releases/{tail}"
            ))?)
        }
        _ => bail!("unsupported release API source"),
    }
}

fn resolve_github_url(
    client: &Client,
    url: &Url,
    original: &str,
    channel: Channel,
    release_selector: Option<&str>,
) -> Result<ResolvedPackage> {
    let web_origin = origin_url(url)?;
    let host = web_origin.host_str().context("GitHub URL has no host")?;
    let api_origin = if host == "github.com" {
        Url::parse("https://api.github.com/")?
    } else {
        web_origin.join("api/v3/")?
    };
    let parts = decoded_path_segments(url)?;
    if parts.len() < 2 {
        bail!("invalid GitHub URL: {original}");
    }
    if parts.len() >= 6
        && parts[2..4] == ["releases", "download"]
        && !parts[..2].iter().any(|part| part.contains(':'))
    {
        let (base_id, owner, app) = forge_identity(host, &parts[..2])?;
        let selector = monorepo_selector(&parts[4]).filter(|selector| selector != &parts[4]);
        let (id, owner, app) = selected_identity(base_id, owner, app, selector.as_deref());
        let source = repository_source(&web_origin, &parts[..2])?;
        return Ok(ResolvedPackage {
            id,
            owner,
            app: app.clone(),
            kind: SourceKind::Github,
            source,
            tag: Some(parts[4].clone()),
            automatic_pin: true,
            pinned: true,
            channel,
            release_selector: selector,
            forge_origin: Some(web_origin.to_string()),
            candidates: vec![AssetCandidate {
                name: parts[5..].join("/"),
                url: url.to_string(),
            }],
        });
    }
    let (project, tag) = if parts.len() == 5
        && parts[2..4] == ["releases", "tag"]
        && !parts[..2].iter().any(|part| part.contains(':'))
    {
        (parts[..2].to_vec(), Some(parts[4].clone()))
    } else if let Some((project, tag)) = tagged_repository_path(url)? {
        if project.len() != 2 {
            bail!("invalid GitHub URL: {original}")
        }
        (project, Some(tag))
    } else if parts.len() == 2 {
        (parts, None)
    } else {
        bail!("invalid GitHub URL: {original}")
    };
    let request = release_selector.map_or_else(
        || release_request(tag.as_deref()),
        |selector| ReleaseRequest::Prefix(selector.to_owned()),
    );
    let source = repository_source(&web_origin, &project)?;
    resolve_github_at(
        client,
        &web_origin,
        &api_origin,
        &project,
        request,
        &source,
        channel,
    )
}

fn parse_repo(input: &str) -> Option<(&str, &str, Option<&str>)> {
    if input.contains("://") {
        return None;
    }
    let (owner, rest) = input.split_once('/')?;
    if owner.is_empty()
        || rest.is_empty()
        || owner
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        return None;
    }
    let (repo, tag) = rest
        .split_once(':')
        .map_or((rest, None), |(repo, tag)| (repo, Some(tag)));
    if repo.is_empty()
        || repo.contains('/')
        || !repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
    {
        return None;
    }
    Some((owner, repo, tag.filter(|tag| !tag.is_empty())))
}

fn resolve_github(
    client: &Client,
    owner: &str,
    repo: &str,
    request: ReleaseRequest,
    source: &str,
    channel: Channel,
) -> Result<ResolvedPackage> {
    let project = [owner.to_owned(), repo.to_owned()];
    resolve_github_at(
        client,
        &Url::parse("https://github.com/")?,
        &Url::parse("https://api.github.com/")?,
        &project,
        request,
        source,
        channel,
    )
}

fn resolve_github_at(
    client: &Client,
    web_origin: &Url,
    api_origin: &Url,
    project: &[String],
    request: ReleaseRequest,
    source: &str,
    channel: Channel,
) -> Result<ResolvedPackage> {
    let [owner, repo] = project else {
        bail!("invalid GitHub project path")
    };
    let allow_prerelease = channel.allows_prereleases();
    let pinned = matches!(request, ReleaseRequest::Exact { .. });
    let mut selector = match &request {
        ReleaseRequest::Prefix(selector) => Some(selector.clone()),
        ReleaseRequest::Exact { selector, .. } => selector.clone(),
        ReleaseRequest::Latest => None,
    };
    let release = match &request {
        ReleaseRequest::Exact { tag, .. } => api_json(
            client,
            release_endpoint(api_origin, SourceKind::Github, project, Some(tag))?,
            SourceKind::Github,
        )?,
        ReleaseRequest::Latest if !allow_prerelease => api_json(
            client,
            release_endpoint(api_origin, SourceKind::Github, project, None)?,
            SourceKind::Github,
        )?,
        ReleaseRequest::Latest => {
            find_github_style_release(client, api_origin, SourceKind::Github, project, None, true)?
        }
        ReleaseRequest::Prefix(selector) => find_github_style_release(
            client,
            api_origin,
            SourceKind::Github,
            project,
            Some(selector),
            allow_prerelease,
        )?,
    };
    if matches!(request, ReleaseRequest::Latest) {
        selector = selector_for_latest(&release.tag_name, repo, source)?;
    }
    let mut candidates = release
        .assets
        .into_iter()
        .map(|asset| AssetCandidate {
            name: asset.name,
            url: asset.browser_download_url,
        })
        .collect::<Vec<_>>();
    filter_and_sort_assets(&mut candidates, &release.tag_name)?;
    if candidates.is_empty() {
        bail!("no supported release assets found for {owner}/{repo}");
    }
    let host = web_origin.host_str().context("GitHub URL has no host")?;
    let (base_id, canonical_owner, app) = forge_identity(host, project)?;
    let (id, canonical_owner, app) =
        selected_identity(base_id, canonical_owner, app, selector.as_deref());
    Ok(ResolvedPackage {
        id,
        owner: canonical_owner,
        app,
        kind: SourceKind::Github,
        source: source.to_owned(),
        tag: Some(release.tag_name),
        automatic_pin: pinned,
        pinned,
        channel,
        release_selector: selector,
        forge_origin: Some(web_origin.to_string()),
        candidates,
    })
}

#[cfg(test)]
fn github_release_endpoint(owner: &str, repo: &str, tag: Option<&str>) -> Result<Url> {
    release_endpoint(
        &Url::parse("https://api.github.com/")?,
        SourceKind::Github,
        &[owner.to_owned(), repo.to_owned()],
        tag,
    )
}

fn api_json<T: for<'de> Deserialize<'de>>(
    client: &Client,
    endpoint: Url,
    kind: SourceKind,
) -> Result<T> {
    let credential_origin = origin_url(&endpoint)?;
    let mut response = send_get(client, endpoint, kind, Some(&credential_origin), true)?;
    if !response.status().is_success() {
        bail!(
            "fetch {} release returned {}",
            kind.as_str(),
            response.status()
        );
    }
    let body = read_limited(&mut response, MAX_API_BODY, "release API response")?;
    serde_json::from_slice(&body).context("decode release API response")
}

fn api_json_page<T: for<'de> Deserialize<'de>>(
    client: &Client,
    endpoint: Url,
    kind: SourceKind,
) -> Result<(T, bool)> {
    let credential_origin = origin_url(&endpoint)?;
    let mut response = send_get(client, endpoint, kind, Some(&credential_origin), true)?;
    let has_next = response_has_next_page(response.headers());
    let body = read_limited(&mut response, MAX_API_BODY, "release API response")?;
    let value = serde_json::from_slice(&body).context("decode release API response")?;
    Ok((value, has_next))
}

fn response_has_next_page(headers: &HeaderMap) -> bool {
    headers
        .get(LINK)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|link| link.contains("rel=\"next\"")))
        || headers
            .get("X-Next-Page")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.is_empty())
}

pub fn asset_response(
    client: &Client,
    package: &ResolvedPackage,
    candidate: &AssetCandidate,
) -> Result<Response> {
    let url = Url::parse(&candidate.url).context("invalid release asset URL")?;
    let origin = if package.kind == SourceKind::Direct {
        Some(origin_url(&url)?)
    } else {
        package
            .forge_origin
            .as_deref()
            .map(Url::parse)
            .transpose()?
    };
    send_get(client, url, package.kind, origin.as_ref(), false)
}

pub fn direct_response(client: &Client, input: &str) -> Result<Response> {
    let url = Url::parse(input).context("invalid direct URL")?;
    let origin = origin_url(&url)?;
    send_get(client, url, SourceKind::Direct, Some(&origin), false)
}

pub fn conditional_head(
    client: &Client,
    input: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Response> {
    let mut url = Url::parse(input).context("invalid stored package URL")?;
    let credential_origin = origin_url(&url)?;
    for redirect_count in 0..=MAX_REDIRECTS {
        let mut request = client.head(url.clone());
        if same_origin(&credential_origin, &url) {
            request = authenticate_for_origin(request, SourceKind::Direct, &credential_origin)?;
        }
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }
        let response = request.send()?;
        if matches!(
            response.status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED | reqwest::StatusCode::NOT_IMPLEMENTED
        ) {
            return conditional_get(client, url, &credential_origin, etag, last_modified);
        }
        if response.status() == reqwest::StatusCode::NOT_MODIFIED
            || !response.status().is_redirection()
        {
            return Ok(response);
        }
        if redirect_count == MAX_REDIRECTS {
            bail!("too many HTTP redirects during conditional HEAD");
        }
        let location = response
            .headers()
            .get(LOCATION)
            .context("redirect response has no Location header")?
            .to_str()
            .context("redirect Location is not valid text")?;
        url = url.join(location).context("invalid redirect Location")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("redirected to unsupported URL scheme: {}", url.scheme());
        }
    }
    unreachable!()
}

fn conditional_get(
    client: &Client,
    mut url: Url,
    credential_origin: &Url,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Response> {
    for redirect_count in 0..=MAX_REDIRECTS {
        let mut request = client.get(url.clone());
        if same_origin(credential_origin, &url) {
            request = authenticate_for_origin(request, SourceKind::Direct, credential_origin)?;
        }
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }
        let response = request.send()?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED
            || !response.status().is_redirection()
        {
            return Ok(response);
        }
        if redirect_count == MAX_REDIRECTS {
            bail!("too many HTTP redirects during conditional GET")
        }
        let location = response
            .headers()
            .get(LOCATION)
            .context("redirect response has no Location header")?
            .to_str()?;
        url = url.join(location)?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("redirected to unsupported URL scheme: {}", url.scheme())
        }
    }
    unreachable!()
}

fn send_get(
    client: &Client,
    mut url: Url,
    kind: SourceKind,
    credential_origin: Option<&Url>,
    api: bool,
) -> Result<Response> {
    for redirect_count in 0..=MAX_REDIRECTS {
        let mut request = client.get(url.clone());
        if let Some(origin) = credential_origin.filter(|origin| same_origin(origin, &url)) {
            request = match kind {
                SourceKind::Github => authenticate_github(request)?,
                SourceKind::Gitea | SourceKind::Gitlab => {
                    authenticate_for_origin(request, kind, origin)?
                }
                SourceKind::Direct => authenticate_for_origin(request, kind, origin)?,
            };
        }
        if api && kind == SourceKind::Github {
            request = request.header(ACCEPT, "application/vnd.github+json");
        }
        let response = request.send()?;
        if !response.status().is_redirection() {
            return response.error_for_status().context("HTTP GET failed");
        }
        if redirect_count == MAX_REDIRECTS {
            bail!("too many HTTP redirects");
        }
        let location = response
            .headers()
            .get(LOCATION)
            .context("redirect response has no Location header")?
            .to_str()
            .context("redirect Location is not valid text")?;
        url = url.join(location).context("invalid redirect Location")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("redirected to unsupported URL scheme: {}", url.scheme());
        }
    }
    unreachable!()
}

fn authenticate_github(
    request: reqwest::blocking::RequestBuilder,
) -> Result<reqwest::blocking::RequestBuilder> {
    let token = std::env::var("EGET_GITHUB_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN"));
    let Ok(token) = token else {
        return Ok(request);
    };
    if token.is_empty() {
        return Ok(request);
    }
    Ok(request.bearer_auth(token))
}

fn authenticate_for_origin(
    request: reqwest::blocking::RequestBuilder,
    kind: SourceKind,
    origin: &Url,
) -> Result<reqwest::blocking::RequestBuilder> {
    let Some(host) = origin.host_str() else {
        return Ok(request);
    };
    let variable = token_env_name(host);
    let Ok(token) = std::env::var(variable) else {
        return Ok(request);
    };
    if token.is_empty() {
        return Ok(request);
    }
    match kind {
        SourceKind::Gitea => {
            let mut value = HeaderValue::from_str(&format!("token {token}"))?;
            value.set_sensitive(true);
            Ok(request.header(AUTHORIZATION, value))
        }
        SourceKind::Gitlab => {
            let mut value = HeaderValue::from_str(&token)?;
            value.set_sensitive(true);
            Ok(request.header("PRIVATE-TOKEN", value))
        }
        SourceKind::Direct => {
            let mut value = HeaderValue::from_str(&token)?;
            value.set_sensitive(true);
            Ok(request.header(AUTHORIZATION, value))
        }
        SourceKind::Github => Ok(request),
    }
}

pub fn token_env_name(host: &str) -> String {
    let ascii = host.to_ascii_lowercase();
    let base = ascii.strip_suffix(".com").unwrap_or(&ascii);
    let fragment = base
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("EGET_{fragment}_TOKEN")
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn origin_url(url: &Url) -> Result<Url> {
    url.host().context("URL has no host")?;
    let mut origin = url.clone();
    origin.set_path("/");
    origin.set_query(None);
    origin.set_fragment(None);
    Ok(origin)
}

fn package_source(url: &Url) -> Result<String> {
    let host = url.host().context("URL has no host")?;
    let mut source = host.to_string().to_ascii_lowercase();
    if let Some(port) = url.port() {
        source.push_str(&format!(":{port}"));
    }
    Ok(source)
}

fn read_limited(response: &mut Response, limit: u64, label: &str) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        bail!("{label} exceeds {limit} bytes");
    }
    let mut bytes = Vec::new();
    response.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("{label} exceeds {limit} bytes");
    }
    Ok(bytes)
}

fn forge_identity(host: &str, project: &[String]) -> Result<(String, String, String)> {
    let app = project
        .last()
        .context("repository path is empty")?
        .to_owned();
    let prefix = project[..project.len() - 1].to_vec();
    let owner = if prefix.is_empty() {
        host.to_owned()
    } else {
        format!("{host}/{}", prefix.join("/"))
    };
    Ok((format!("{owner}/{app}"), owner, app))
}

fn decoded_path_segments(url: &Url) -> Result<Vec<String>> {
    url.path_segments()
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .map(|part| {
            percent_decode_str(part)
                .decode_utf8()
                .map(|part| part.into_owned())
                .context("URL path is not valid UTF-8")
        })
        .collect()
}

fn url_asset_name(url: &Url, fallback: &str) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .map(|segment| percent_decode_str(segment).decode_utf8_lossy().into_owned())
        .unwrap_or_else(|| fallback.to_owned())
}

fn filter_and_sort_assets(candidates: &mut Vec<AssetCandidate>, tag: &str) -> Result<()> {
    let platform = current_platform()?;
    candidates.retain(|candidate| supported_asset(&candidate.name, tag, platform));
    candidates.sort_by_key(|candidate| Reverse(asset_score(&candidate.name, platform)));
    Ok(())
}

const ARCHIVE_SUFFIXES: &[&str] = &[
    ".7z", ".zip", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz", ".tbz2", ".tar.xz", ".txz",
    ".tar.zst", ".tzst", ".gz", ".bz2", ".xz", ".zst",
];

fn supported_asset(name: &str, tag: &str, platform: Platform) -> bool {
    let name = name.to_ascii_lowercase();
    let tag = tag.to_ascii_lowercase();
    if [".sig", ".asc", ".minisig", ".sha256", ".sha512", ".shasum"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
        || ["checksum", "checksums", "source"]
            .iter()
            .any(|word| marker_match(&name, word))
    {
        return false;
    }
    if !os_markers(platform.os())
        .iter()
        .any(|marker| marker_match(&name, marker))
        || !arch_markers(platform)
            .iter()
            .any(|marker| marker_match(&name, marker))
    {
        return false;
    }
    (!tag.is_empty() && name.ends_with(&tag))
        || ARCHIVE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        || terminal_platform_pair(&name, platform)
        || platform_suffixes(platform)
            .iter()
            .any(|suffix| name.ends_with(suffix))
        || Path::new(&name).extension().is_none()
}

fn asset_score(name: &str, platform: Platform) -> i32 {
    let name = name.to_ascii_lowercase();
    let mut score = i32::from(ARCHIVE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))) * 10;
    match platform {
        Platform::Linux { libc, .. } => {
            if marker_match(&name, "static") {
                score += 5;
            }
            match libc {
                Some(Libc::Glibc)
                    if ["glibc", "gnu"]
                        .iter()
                        .any(|marker| marker_match(&name, marker)) =>
                {
                    score += 20
                }
                Some(Libc::Glibc) if marker_match(&name, "musl") => score -= 1,
                Some(Libc::Musl) if marker_match(&name, "musl") => score += 20,
                Some(Libc::Musl)
                    if ["glibc", "gnu"]
                        .iter()
                        .any(|marker| marker_match(&name, marker)) =>
                {
                    score -= 1
                }
                _ => {}
            }
        }
        Platform::Macos { .. } => {}
    }
    score
}

#[cfg(target_os = "linux")]
fn current_platform() -> Result<Platform> {
    let host = compat::Host::current()?;
    Ok(Platform::Linux {
        arch: host.arch,
        libc: detect_libc(),
    })
}

#[cfg(target_os = "macos")]
fn current_platform() -> Result<Platform> {
    let host = compat::Host::current()?;
    Ok(Platform::Macos { arch: host.arch })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn current_platform() -> Result<Platform> {
    anyhow::bail!("unsupported operating system")
}

#[cfg(target_os = "linux")]
fn detect_libc() -> Option<Libc> {
    if let Ok(executable) = std::env::current_exe()
        && let Ok(Some(interpreter)) = compat::elf_interpreter_path(&executable)
    {
        if interpreter.contains("musl") {
            return Some(Libc::Musl);
        }
        if interpreter.contains("ld-linux") {
            return Some(Libc::Glibc);
        }
    }
    for directory in ["/lib", "/lib64"] {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        let names = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if names.iter().any(|name| name.starts_with("ld-linux")) {
            return Some(Libc::Glibc);
        }
        if names.iter().any(|name| name.starts_with("ld-musl")) {
            return Some(Libc::Musl);
        }
    }
    None
}

fn os_markers(os: HostOs) -> &'static [&'static str] {
    match os {
        HostOs::Linux => &["linux"],
        HostOs::Macos => &["mac", "macos", "darwin"],
    }
}

fn arch_markers(platform: Platform) -> &'static [&'static str] {
    match (platform.os(), platform.arch()) {
        (HostOs::Linux, HostArch::X86_64) => &["amd64", "x86_64", "x64", "linux64"],
        (HostOs::Macos, HostArch::X86_64) => {
            &["amd64", "x86_64", "x64", "mac64", "macos64", "darwin64"]
        }
        (_, HostArch::Aarch64) => &["arm64", "aarch64"],
    }
}

fn platform_suffixes(platform: Platform) -> Vec<String> {
    arch_markers(platform)
        .iter()
        .map(|marker| format!(".{marker}"))
        .collect()
}

fn terminal_platform_pair(name: &str, platform: Platform) -> bool {
    os_markers(platform.os()).iter().any(|os| {
        arch_markers(platform).iter().any(|arch| {
            terminal_marker_pair(name, os, arch) || terminal_marker_pair(name, arch, os)
        })
    })
}

fn terminal_marker_pair(name: &str, first: &str, second: &str) -> bool {
    let Some(before_second) = name.strip_suffix(second) else {
        return false;
    };
    let before_separator = before_second.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    if before_separator.len() == before_second.len() {
        return false;
    }
    let Some(before_first) = before_separator.strip_suffix(first) else {
        return false;
    };
    before_first
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_ascii_alphanumeric())
}

fn marker_match(name: &str, marker: &str) -> bool {
    name.match_indices(marker)
        .any(|(index, _)| index == 0 || !name.as_bytes()[index - 1].is_ascii_alphanumeric())
}

fn normalized_owner(host: &str) -> String {
    if host.bytes().filter(|byte| *byte == b'.').count() < 2 {
        return host.to_owned();
    }
    let Some((label, rest)) = host.split_once('.') else {
        return host.to_owned();
    };
    let keyword = label.trim_end_matches(|character: char| character.is_ascii_digit());
    if matches!(
        keyword,
        "www"
            | "download"
            | "downloads"
            | "dl"
            | "cache"
            | "cdn"
            | "release"
            | "releases"
            | "assets"
            | "static"
            | "ftp"
    ) {
        rest.to_owned()
    } else {
        host.to_owned()
    }
}

fn direct_app(url: &Url) -> String {
    let segments = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    segments
        .last()
        .and_then(|segment| normalized_app_segment(segment))
        .unwrap_or_else(|| "default".to_owned())
}

fn direct_url_has_version(url: &Url) -> bool {
    DIRECT_URL_VERSION.is_match(url.path())
}

const PLATFORM_NAME_MARKERS: &[&str] = &[
    "linux", "win", "windows", "mac", "macos", "darwin", "amd64", "x86_64", "x64", "linux64",
    "mac64", "macos64", "darwin64", "arm64", "aarch64", "musl", "glibc", "gnu", "static", "exe",
];

fn normalized_app_segment(segment: &str) -> Option<String> {
    let mut name = segment.to_ascii_lowercase();
    if let Some(suffix) = ARCHIVE_SUFFIXES
        .iter()
        .filter(|suffix| name.ends_with(**suffix))
        .max_by_key(|suffix| suffix.len())
    {
        name.truncate(name.len() - suffix.len());
    }
    name = name
        .chars()
        .take_while(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .collect::<String>()
        .trim_matches(['-', '_', '.'])
        .to_owned();
    if !name.starts_with(|character: char| character.is_ascii_alphabetic()) {
        return None;
    }
    if let Some(index) = name
        .match_indices(['-', '_', '.'])
        .find_map(|(index, _)| removable_artifact_suffix(&name[index + 1..]).then_some(index))
    {
        name.truncate(index);
    }
    let name = name.trim_end_matches(['-', '_', '.']);
    (!name.is_empty()).then(|| name.to_owned())
}

fn removable_artifact_suffix(suffix: &str) -> bool {
    let mut chars = suffix.chars();
    if chars
        .next()
        .is_some_and(|character| character.is_ascii_digit())
    {
        return true;
    }
    if suffix
        .strip_prefix('v')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|character| character.is_ascii_digit())
    {
        return true;
    }
    PLATFORM_NAME_MARKERS.iter().any(|marker| {
        suffix == *marker
            || suffix
                .strip_prefix(marker)
                .is_some_and(|rest| rest.starts_with(['-', '_', '.']))
    })
}

pub fn redact(url: &Url) -> String {
    let mut clean = url.clone();
    clean.set_username("").ok();
    clean.set_password(None).ok();
    clean.set_query(None);
    clean.set_fragment(None);
    clean.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::thread;

    static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct MockResponse {
        status: &'static str,
        headers: Vec<(&'static str, &'static str)>,
        body: String,
    }

    fn serve(responses: Vec<MockResponse>) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 8192];
                let read = stream.read(&mut request).unwrap();
                requests.push(String::from_utf8_lossy(&request[..read]).into_owned());
                write!(stream, "HTTP/1.1 {}\r\n", response.status).unwrap();
                for (name, value) in response.headers {
                    write!(stream, "{name}: {value}\r\n").unwrap();
                }
                write!(
                    stream,
                    "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.body.len(),
                    response.body
                )
                .unwrap();
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    fn platform_asset_name() -> &'static str {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => "tool-darwin-arm64",
            ("macos", _) => "tool-darwin-amd64",
            (_, "aarch64") => "tool-linux-arm64",
            _ => "tool-linux-amd64",
        }
    }

    fn dotted_platform_asset_name() -> &'static str {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => "direnv.darwin-arm64",
            ("macos", _) => "direnv.darwin-amd64",
            (_, "aarch64") => "direnv.linux-arm64",
            _ => "direnv.linux-amd64",
        }
    }

    #[test]
    fn known_hosts_and_token_names_follow_contract() {
        assert_eq!(known_forge("gitea.com"), Some(SourceKind::Gitea));
        assert_eq!(known_forge("gitlab.com"), Some(SourceKind::Gitlab));
        assert_eq!(known_forge("example.com"), None);
        assert_eq!(token_env_name("gitea.com"), "EGET_GITEA_TOKEN");
        assert_eq!(
            token_env_name("gitlab.acmecorp.io"),
            "EGET_GITLAB_ACMECORP_IO_TOKEN"
        );
    }

    #[test]
    fn url_normalization_uses_ascii_idn_hosts() {
        let url = normalized_url("https://BÜCHER.example/Owner/Tool").unwrap();
        assert_eq!(url.host_str(), Some("xn--bcher-kva.example"));
    }

    #[test]
    fn parses_gitea_and_gitlab_release_shapes() {
        let gitea =
            normalized_url("https://gitea.com/Owner/Tool/releases/download/v1/tool-linux-amd64")
                .unwrap();
        assert!(
            matches!(parse_forge_url(&gitea, SourceKind::Gitea).unwrap(), ForgeInput::Direct { tag, .. } if tag == "v1")
        );
        let gitlab = normalized_url("https://gitlab.com/Group/Sub/Tool/-/releases/v%2F1").unwrap();
        assert!(
            matches!(parse_forge_url(&gitlab, SourceKind::Gitlab).unwrap(), ForgeInput::Repository { project, tag: Some(tag) } if project == ["Group", "Sub", "Tool"] && tag == "v/1")
        );
    }

    #[test]
    fn parses_host_qualified_repository_tags() {
        let gitea = normalized_url("gitea.com/Owner/Tool:release/1.2").unwrap();
        assert!(
            matches!(parse_forge_url(&gitea, SourceKind::Gitea).unwrap(), ForgeInput::Repository { project, tag: Some(tag) } if project == ["Owner", "Tool"] && tag == "release/1.2")
        );

        let gitlab = normalized_url("https://gitlab.com/Group/Sub/Tool:release/1.2").unwrap();
        assert!(
            matches!(parse_forge_url(&gitlab, SourceKind::Gitlab).unwrap(), ForgeInput::Repository { project, tag: Some(tag) } if project == ["Group", "Sub", "Tool"] && tag == "release/1.2")
        );

        let github = normalized_url("github.com/Owner/Tool:release/1.2").unwrap();
        let (project, tag) = tagged_repository_path(&github).unwrap().unwrap();
        assert_eq!(tag, "release/1.2");
        assert_eq!(project, ["Owner", "Tool"]);
        assert_eq!(
            github_release_endpoint(&project[0], &project[1], Some(&tag))
                .unwrap()
                .as_str(),
            "https://api.github.com/repos/Owner/Tool/releases/tags/release%2F1%2E2"
        );

        let encoded = normalized_url("gitea.com/Owner/Tool:v%2F1").unwrap();
        assert!(
            matches!(parse_forge_url(&encoded, SourceKind::Gitea).unwrap(), ForgeInput::Repository { tag: Some(tag), .. } if tag == "v/1")
        );

        let route_shaped = normalized_url("gitea.com/Owner/Tool:feature/releases/tag/v1").unwrap();
        assert!(
            matches!(parse_forge_url(&route_shaped, SourceKind::Gitea).unwrap(), ForgeInput::Repository { tag: Some(tag), .. } if tag == "feature/releases/tag/v1")
        );
        let route_shaped = normalized_url("gitlab.com/Group/Tool:feature/-/releases/v1").unwrap();
        assert!(
            matches!(parse_forge_url(&route_shaped, SourceKind::Gitlab).unwrap(), ForgeInput::Repository { tag: Some(tag), .. } if tag == "feature/-/releases/v1")
        );
    }

    #[test]
    fn local_identity_hints_cover_tracking_exact_and_self_hosted_inputs() {
        assert_eq!(
            package_identity_hint("Owner/Repo", None).unwrap(),
            PackageIdentityHint {
                ids: vec![
                    "github.com/Owner/Repo".into(),
                    "github.com/Owner/Repo:repo".into(),
                ],
                exact: false,
            }
        );
        assert_eq!(
            package_identity_hint("Owner/Repo:tool/v2", None).unwrap(),
            PackageIdentityHint {
                ids: vec!["github.com/Owner/Repo:tool".into()],
                exact: true,
            }
        );
        assert_eq!(
            package_identity_hint(
                "https://forge.example/Group/Tool:v2",
                Some(SourceKind::Gitlab)
            )
            .unwrap(),
            PackageIdentityHint {
                ids: vec!["forge.example/Group/Tool".into()],
                exact: true,
            }
        );
        assert_eq!(
            package_identity_hint(
                "https://downloads.example/tool-v2-linux-amd64.tar.gz",
                Some(SourceKind::Direct)
            )
            .unwrap()
            .ids,
            ["downloads.example/tool"]
        );
        assert_eq!(
            package_identity_hint(
                "https://downloads.example:8443/tool-v2-linux-amd64.tar.gz",
                Some(SourceKind::Direct)
            )
            .unwrap()
            .ids,
            ["downloads.example:8443/tool"]
        );
    }

    #[test]
    fn rejects_empty_host_qualified_repository_tags() {
        for (input, kind) in [
            ("https://gitea.com/Owner/Tool:", SourceKind::Gitea),
            ("https://gitea.com/Owner/:v1", SourceKind::Gitea),
            ("https://gitlab.com/Group/Tool:", SourceKind::Gitlab),
            ("https://gitlab.com/:v1", SourceKind::Gitlab),
        ] {
            let url = normalized_url(input).unwrap();
            assert!(parse_forge_url(&url, kind).is_err());
        }

        let github = normalized_url("https://github.com/Owner/Tool:").unwrap();
        assert!(
            resolve_github_url(
                &client().unwrap(),
                &github,
                github.as_str(),
                Channel::Stable,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn classifies_exact_tags_and_monorepo_selectors() {
        assert_eq!(
            release_request(Some("v1.2.3")),
            ReleaseRequest::Exact {
                tag: "v1.2.3".into(),
                selector: None,
            }
        );
        assert_eq!(
            release_request(Some("r1.2.3")),
            ReleaseRequest::Exact {
                tag: "r1.2.3".into(),
                selector: None,
            }
        );
        assert_eq!(
            release_request(Some("tool/x1.2.3")),
            ReleaseRequest::Exact {
                tag: "tool/x1.2.3".into(),
                selector: Some("tool".into()),
            }
        );
        assert_eq!(
            release_request(Some("2006-01-11v1234")),
            ReleaseRequest::Exact {
                tag: "2006-01-11v1234".into(),
                selector: None,
            }
        );
        assert_eq!(
            release_request(Some("gnu-sed-4.10")),
            ReleaseRequest::Exact {
                tag: "gnu-sed-4.10".into(),
                selector: Some("gnu-sed".into()),
            }
        );
        assert_eq!(
            release_request(Some("kustomize/v5.8.1")),
            ReleaseRequest::Exact {
                tag: "kustomize/v5.8.1".into(),
                selector: Some("kustomize".into()),
            }
        );
        assert_eq!(
            release_request(Some("gnu-sed")),
            ReleaseRequest::Prefix("gnu-sed".into())
        );
        assert!(selector_matches("gnu-sed-4.10", "gnu-sed"));
        assert!(!selector_matches("gnu-sed2-4.10", "gnu-sed"));
        assert!(!selector_matches("GNU-sed-4.10", "gnu-sed"));
    }

    #[test]
    fn repository_named_latest_selector_is_case_sensitive() {
        let selector = selector_for_latest("jq-1.8.2", "jq", "jqlang/jq").unwrap();
        assert_eq!(selector.as_deref(), Some("jq"));
        let (id, owner, app) =
            forge_identity("github.com", &["jqlang".into(), "jq".into()]).unwrap();
        let (id, _, app) = selected_identity(id, owner, app, selector.as_deref());
        assert_eq!(id, "github.com/jqlang/jq:jq");
        assert_eq!(app, "jq");

        for (tag, repository) in [("tool/1.2.3", "repo"), ("jq-1.8.2", "JQ")] {
            let error = selector_for_latest(tag, repository, "owner/repo").unwrap_err();
            assert!(error.downcast_ref::<MonorepoLatest>().is_some());
        }
    }

    #[test]
    fn repository_named_latest_tracks_subsequent_prefix_releases() {
        let release = |tag: &str| {
            format!(
                r#"{{"tag_name":"{tag}","assets":[{{"name":"{}","browser_download_url":"https://example.com/asset"}}]}}"#,
                platform_asset_name()
            )
        };
        let list = |tag: &str| format!("[{}]", release(tag));
        let (base, handle) = serve(vec![
            MockResponse {
                status: "200 OK",
                headers: vec![],
                body: release("jq-1.8.2"),
            },
            MockResponse {
                status: "200 OK",
                headers: vec![],
                body: list("jq-1.8.3"),
            },
        ]);
        let input = format!("{base}/jqlang/jq");
        let package = resolve_with_preferences(
            &client().unwrap(),
            &input,
            Some(SourceKind::Gitea),
            Channel::Stable,
            None,
        )
        .unwrap();
        assert_eq!(package.id, format!("{}/jqlang/jq:jq", &base[7..]));
        assert_eq!(package.app, "jq");
        assert_eq!(package.tag.as_deref(), Some("jq-1.8.2"));
        assert_eq!(package.release_selector.as_deref(), Some("jq"));
        assert!(!package.automatic_pin);

        let update = resolve_with_preferences(
            &client().unwrap(),
            &package.source,
            Some(SourceKind::Gitea),
            package.channel,
            package.release_selector.as_deref(),
        )
        .unwrap();
        assert_eq!(update.id, package.id);
        assert_eq!(update.tag.as_deref(), Some("jq-1.8.3"));

        let requests = handle.join().unwrap();
        assert!(requests[0].contains("/api/v1/repos/jqlang/jq/releases/latest"));
        assert!(requests[1].contains("/api/v1/repos/jqlang/jq/releases?limit=100&page=1"));
    }

    #[test]
    fn gitea_prefix_resolution_searches_three_pages_with_boundaries() {
        let release = |tag: &str, prerelease: bool| {
            format!(
                r#"[{{"tag_name":"{tag}","draft":false,"prerelease":{prerelease},"assets":[{{"name":"{}","browser_download_url":"https://example.com/asset"}}]}}]"#,
                platform_asset_name()
            )
        };
        let (base, handle) = serve(vec![
            MockResponse {
                status: "200 OK",
                headers: vec![("X-Next-Page", "2")],
                body: release("gnu-sed2-5.0", false),
            },
            MockResponse {
                status: "200 OK",
                headers: vec![("X-Next-Page", "3")],
                body: release("gnu-sed-5.0-rc1", true),
            },
            MockResponse {
                status: "200 OK",
                headers: vec![],
                body: release("gnu-sed-4.10", false),
            },
        ]);
        let package = resolve_with_preferences(
            &client().unwrap(),
            &format!("{base}/Owner/Tool:gnu-sed"),
            Some(SourceKind::Gitea),
            Channel::Stable,
            None,
        )
        .unwrap();
        assert_eq!(package.id, format!("{}/Owner/Tool:gnu-sed", &base[7..]));
        assert_eq!(package.tag.as_deref(), Some("gnu-sed-4.10"));
        assert_eq!(package.release_selector.as_deref(), Some("gnu-sed"));
        assert!(!package.automatic_pin);
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].contains("/releases?limit=100&page=1"));
        assert!(requests[2].contains("/releases?limit=100&page=3"));
    }

    #[test]
    fn prerelease_option_makes_matching_prerelease_eligible() {
        let (base, handle) = serve(vec![MockResponse {
            status: "200 OK",
            headers: vec![],
            body: format!(
                r#"[{{"tag_name":"tool-2.0-rc1","draft":false,"prerelease":true,"assets":[{{"name":"{}","browser_download_url":"https://example.com/asset"}}]}}]"#,
                platform_asset_name()
            ),
        }]);
        let package = resolve_with_preferences(
            &client().unwrap(),
            &format!("{base}/Owner/Repo:tool"),
            Some(SourceKind::Gitea),
            Channel::Prerelease,
            None,
        )
        .unwrap();
        assert_eq!(package.tag.as_deref(), Some("tool-2.0-rc1"));
        assert_eq!(package.channel, Channel::Prerelease);
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn unqualified_monorepo_latest_is_rejected_before_installation() {
        let (base, handle) = serve(vec![MockResponse {
            status: "200 OK",
            headers: vec![],
            body: format!(
                r#"{{"tag_name":"tool/1.2.3","assets":[{{"name":"{}","browser_download_url":"https://example.com/asset"}}]}}"#,
                platform_asset_name()
            ),
        }]);
        let error = resolve_with_preferences(
            &client().unwrap(),
            &format!("{base}/Owner/Tool"),
            Some(SourceKind::Gitea),
            Channel::Stable,
            None,
        )
        .unwrap_err();
        let guard = error.downcast_ref::<MonorepoLatest>().unwrap();
        assert_eq!(guard.selector, "tool");
        assert!(error.to_string().contains("eget install"));
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn exact_monorepo_tag_uses_selector_specific_identity() {
        let (base, handle) = serve(vec![MockResponse {
            status: "200 OK",
            headers: vec![],
            body: format!(
                r#"{{"tag_name":"repo-1.2.3","assets":[{{"name":"{}","browser_download_url":"https://example.com/asset"}}]}}"#,
                platform_asset_name()
            ),
        }]);
        let package = resolve_with_preferences(
            &client().unwrap(),
            &format!("{base}/Owner/Repo:repo-1.2.3"),
            Some(SourceKind::Gitea),
            Channel::Stable,
            None,
        )
        .unwrap();
        assert_eq!(package.id, format!("{}/Owner/Repo:repo", &base[7..]));
        assert_eq!(package.app, "Repo");
        assert!(package.automatic_pin);
        let requests = handle.join().unwrap();
        assert!(requests[0].contains("/releases/tags/repo-1.2.3"));
    }

    #[test]
    fn direct_ids_are_stable_and_redacted() {
        let url = normalized_url("https://token:secret@downloads.example.com/tool-linux?key=nope")
            .unwrap();
        let app = direct_app(&url);
        let owner = normalized_owner(url.host_str().unwrap());
        assert_eq!(format!("{owner}/{app}"), "example.com/tool");
        assert!(!redact(&url).contains("secret"));
        assert!(!redact(&url).contains("key"));
    }

    #[test]
    fn direct_url_versions_are_detected_only_in_the_path_at_boundaries() {
        for input in [
            "https://go.dev/dl/go1.25.0.linux-amd64.tar.gz",
            "https://cache.agilebits.com/dist/1P/op2/pkg/v2.35.0/op_linux_amd64_v2.35.0.zip",
            "https://example.com/tool-1.2.3-linux",
            "https://example.com/versions/1.2.3/tool",
            "https://example.com/tool_1.2.3",
        ] {
            assert!(
                direct_url_has_version(&normalized_url(input).unwrap()),
                "{input}"
            );
        }

        for input in [
            "https://1.2.3.4/tool",
            "https://example.com/tool?version=1.2.3",
            "https://example.com/tool-1.2",
            "https://example.com/tool-1.2.3beta",
        ] {
            assert!(
                !direct_url_has_version(&normalized_url(input).unwrap()),
                "{input}"
            );
        }
    }

    #[test]
    fn versioned_direct_urls_request_automatic_pinning() {
        let package = resolve_with_preferences(
            &client().unwrap(),
            "https://example.com/tool-v1.2.3",
            Some(SourceKind::Direct),
            Channel::Stable,
            None,
        )
        .unwrap();
        assert!(package.automatic_pin);
        assert!(package.pinned);
    }

    #[test]
    fn direct_app_removes_executable_packaging_marker() {
        let url =
            normalized_url("https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip").unwrap();
        assert_eq!(direct_app(&url), "awscli");
        assert_eq!(
            normalized_owner(url.host_str().unwrap()),
            "awscli.amazonaws.com"
        );
        assert_eq!(
            direct_app(&normalized_url("https://example.com/tool-linux-amd64.7z").unwrap()),
            "tool"
        );
        assert_eq!(
            direct_app(&normalized_url("https://example.com").unwrap()),
            "default"
        );
        assert_eq!(
            normalized_url("https://example.com//one///tool")
                .unwrap()
                .path(),
            "/one/tool"
        );
    }

    #[test]
    fn direct_owner_ignores_numbered_python_policy_subdomains() {
        assert_eq!(normalized_owner("www42.example.com"), "example.com");
        assert_eq!(normalized_owner("downloads2.example.com"), "example.com");
        assert_eq!(normalized_owner("other.example.com"), "other.example.com");
    }

    #[test]
    fn assets_require_a_supported_or_extensionless_host_payload() {
        let platform = Platform::Linux {
            arch: HostArch::X86_64,
            libc: Some(Libc::Glibc),
        };
        assert!(supported_asset("tool-linux-amd64.tar.gz", "v1", platform));
        assert!(supported_asset("tool-linux-amd64.7z", "v1", platform));
        assert!(supported_asset("tool-linux-amd64", "v1", platform));
        assert!(!supported_asset("tool-windows-amd64.zip", "v1", platform));
        assert!(!supported_asset("checksums-linux-amd64", "v1", platform));
    }

    #[test]
    fn raw_assets_accept_terminal_platform_pairs_in_either_order() {
        let linux = Platform::Linux {
            arch: HostArch::X86_64,
            libc: Some(Libc::Glibc),
        };
        for name in [
            "direnv.linux-amd64",
            "tool.linux.x64",
            "tool.linux.x86_64",
            "tool.linux_amd64",
            "tool.amd64_linux",
        ] {
            assert!(supported_asset(name, "v1", linux), "rejected {name}");
        }
        assert!(supported_asset("tool.linux64", "v1", linux));
        assert!(!supported_asset("tool.notlinux-amd64", "v1", linux));
        assert!(!supported_asset("tool.darwin-amd64", "v1", linux));
        assert!(!supported_asset("tool.linux-arm64", "v1", linux));
        assert!(!supported_asset("direnv.linux-amd64.txt", "v1", linux));

        let macos = Platform::Macos {
            arch: HostArch::Aarch64,
        };
        assert!(supported_asset("tool.darwin-arm64", "v1", macos));
        assert!(supported_asset("tool.arm64_darwin", "v1", macos));
    }

    #[test]
    fn github_release_keeps_a_terminal_platform_pair_asset() {
        let asset = dotted_platform_asset_name();
        let (base, handle) = serve(vec![MockResponse {
            status: "200 OK",
            headers: vec![],
            body: format!(
                r#"{{"tag_name":"v2.37.1","assets":[{{"name":"{asset}","browser_download_url":"https://example.com/{asset}"}}]}}"#
            ),
        }]);
        let package = resolve_github_at(
            &client().unwrap(),
            &Url::parse("https://github.com/").unwrap(),
            &Url::parse(&format!("{base}/")).unwrap(),
            &["direnv".into(), "direnv".into()],
            ReleaseRequest::Latest,
            "github.com/direnv/direnv",
            Channel::Stable,
        )
        .unwrap();

        assert_eq!(package.tag.as_deref(), Some("v2.37.1"));
        assert_eq!(package.candidates[0].name, asset);
        assert!(handle.join().unwrap()[0].contains("/repos/direnv/direnv/releases/latest"));
    }

    #[test]
    fn tag_stamped_bare_executables_are_supported_case_insensitively() {
        let platform = Platform::Linux {
            arch: HostArch::X86_64,
            libc: Some(Libc::Glibc),
        };
        let tag = "RELEASE.2025-08-13T08-35-41Z";
        assert!(supported_asset(
            "mc.linux-amd64.release.2025-08-13t08-35-41z",
            tag,
            platform
        ));
        assert!(!supported_asset(
            "mc.linux-amd64.RELEASE.2025-08-13T08-35-41Z.asc",
            tag,
            platform
        ));
    }

    #[test]
    fn glibc_hosts_prefer_glibc_and_gnu_assets() {
        let platform = Platform::Linux {
            arch: HostArch::X86_64,
            libc: Some(Libc::Glibc),
        };
        let musl = asset_score("tool-linux-amd64-musl.tar.gz", platform);
        assert!(asset_score("tool-linux-amd64-glibc.tar.gz", platform) > musl);
        assert!(asset_score("tool-linux-amd64-gnu.tar.gz", platform) > musl);
    }

    #[test]
    fn glibc_hosts_prefer_unmarked_assets_over_musl_assets() {
        let platform = Platform::Linux {
            arch: HostArch::X86_64,
            libc: Some(Libc::Glibc),
        };
        assert!(
            asset_score("opencode-linux-x64.tar.gz", platform)
                > asset_score("opencode-linux-x64-musl.tar.gz", platform)
        );
    }

    #[test]
    fn musl_hosts_prefer_musl_assets() {
        let platform = Platform::Linux {
            arch: HostArch::X86_64,
            libc: Some(Libc::Musl),
        };
        assert!(
            asset_score("tool-linux-amd64-musl.tar.gz", platform)
                > asset_score("tool-linux-amd64-glibc.tar.gz", platform)
        );
    }

    #[test]
    fn musl_hosts_prefer_unmarked_assets_over_glibc_assets() {
        let platform = Platform::Linux {
            arch: HostArch::X86_64,
            libc: Some(Libc::Musl),
        };
        let unmarked = asset_score("tool-linux-amd64.tar.gz", platform);
        assert!(unmarked > asset_score("tool-linux-amd64-glibc.tar.gz", platform));
        assert!(unmarked > asset_score("tool-linux-amd64-gnu.tar.gz", platform));
    }

    #[test]
    fn static_assets_are_preferred_only_on_linux() {
        let linux = Platform::Linux {
            arch: HostArch::X86_64,
            libc: None,
        };
        let macos = Platform::Macos {
            arch: HostArch::X86_64,
        };
        assert!(
            asset_score("tool-linux-amd64-static.tar.gz", linux)
                > asset_score("tool-linux-amd64.tar.gz", linux)
        );
        assert_eq!(
            asset_score("tool-darwin-amd64-static.tar.gz", macos),
            asset_score("tool-darwin-amd64.tar.gz", macos)
        );
    }

    #[test]
    fn macos_scoring_ignores_linux_compatibility_markers() {
        let platform = Platform::Macos {
            arch: HostArch::Aarch64,
        };
        let baseline = asset_score("tool-darwin-arm64.tar.gz", platform);
        for marker in ["glibc", "gnu", "musl", "static"] {
            assert_eq!(
                asset_score(&format!("tool-darwin-arm64-{marker}.tar.gz"), platform),
                baseline
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_platform_is_linux_on_linux() {
        assert!(matches!(
            current_platform().unwrap(),
            Platform::Linux { .. }
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn current_platform_is_macos_on_macos() {
        assert!(matches!(
            current_platform().unwrap(),
            Platform::Macos { .. }
        ));
    }

    #[test]
    fn gitlab_external_release_links_bypass_the_html_warning_page() {
        let origin = Url::parse("https://gitlab.com/").unwrap();
        let external = gitlab_asset_candidate(
            GitlabLink {
                name: "binary: Linux amd64".into(),
                url: "https://gitlab-docker-machine-downloads.s3.amazonaws.com/v0.16.2-gitlab.51/docker-machine-Linux-x86_64".into(),
                direct_asset_url: Some("https://gitlab.com/gitlab-org/ci-cd/docker-machine/-/releases/v0.16.2-gitlab.51/downloads/docker-machine-Linux-x86_64".into()),
            },
            &origin,
        );
        assert_eq!(
            external.url,
            "https://gitlab-docker-machine-downloads.s3.amazonaws.com/v0.16.2-gitlab.51/docker-machine-Linux-x86_64"
        );

        let same_origin = gitlab_asset_candidate(
            GitlabLink {
                name: "tool-linux-amd64".into(),
                url: "https://gitlab.com/group/tool/-/blob/main/tool-linux-amd64".into(),
                direct_asset_url: Some(
                    "https://gitlab.com/group/tool/-/releases/v1/downloads/tool-linux-amd64".into(),
                ),
            },
            &origin,
        );
        assert_eq!(
            same_origin.url,
            "https://gitlab.com/group/tool/-/releases/v1/downloads/tool-linux-amd64"
        );
    }

    #[test]
    fn gitea_probe_and_latest_api_use_exact_paths_and_cache_detection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let release = format!(
            r#"{{"tag_name":"Tool-v1","assets":[{{"name":"{}","browser_download_url":"{base}/asset"}}]}}"#,
            platform_asset_name()
        );
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in [
                ("200 OK", r#"{"version":"1.24"}"#.to_owned()),
                ("200 OK", release.clone()),
                ("200 OK", release),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 8192];
                let read = stream.read(&mut request).unwrap();
                requests.push(String::from_utf8_lossy(&request[..read]).into_owned());
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });

        let temp = tempfile::tempdir().unwrap();
        let database = Database::open(&temp.path().join("eget.sqlite3"), temp.path()).unwrap();
        let client = client().unwrap();
        let first = resolve_with_store(
            &client,
            &database,
            &format!("{base}/Owner/Tool"),
            Channel::Stable,
            None,
        )
        .unwrap();
        let second = resolve_with_store(
            &client,
            &database,
            &format!("{base}/Owner/Tool"),
            Channel::Stable,
            None,
        )
        .unwrap();
        assert_eq!(first.kind, SourceKind::Gitea);
        assert_eq!(first.id, format!("{}/Owner/Tool:Tool", &base[7..]));
        assert_eq!(first.release_selector.as_deref(), Some("Tool"));
        assert_eq!(second.tag.as_deref(), Some("Tool-v1"));
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
        assert!(requests[1].starts_with("GET /api/v1/repos/Owner/Tool/releases/latest HTTP/1.1"));
        assert!(requests[2].starts_with("GET /api/v1/repos/Owner/Tool/releases/latest HTTP/1.1"));
    }

    #[test]
    fn probed_gitea_repository_tag_uses_exact_release_api() {
        let (base, handle) = serve(vec![
            MockResponse {
                status: "200 OK",
                headers: vec![],
                body: r#"{"version":"1.24"}"#.into(),
            },
            MockResponse {
                status: "200 OK",
                headers: vec![],
                body: format!(
                    r#"{{"tag_name":"v/1","assets":[{{"name":"{}","browser_download_url":"https://example.com/asset"}}]}}"#,
                    platform_asset_name()
                ),
            },
        ]);
        let package = resolve(&client().unwrap(), &format!("{base}/Owner/Tool:v/1")).unwrap();
        assert_eq!(package.id, format!("{}/Owner/Tool", &base[7..]));
        assert_eq!(package.tag.as_deref(), Some("v/1"));
        assert!(package.automatic_pin);
        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
        assert!(
            requests[1].starts_with("GET /api/v1/repos/Owner/Tool/releases/tags/v%2F1 HTTP/1.1")
        );
    }

    #[test]
    fn gitlab_probe_header_wins_and_nested_project_is_encoded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let release = format!(
            r#"{{"tag_name":"Tool-v2","assets":{{"links":[{{"name":"{}","url":"{base}/fallback","direct_asset_url":"{base}/direct"}}]}}}}"#,
            platform_asset_name()
        );
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, headers, body) in [
                (
                    "500 Internal Server Error",
                    vec![("X-GitLab-Meta", "yes")],
                    String::new(),
                ),
                ("200 OK", Vec::new(), release),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 8192];
                let read = stream.read(&mut request).unwrap();
                requests.push(String::from_utf8_lossy(&request[..read]).into_owned());
                write!(stream, "HTTP/1.1 {status}\r\n").unwrap();
                for (name, value) in headers {
                    write!(stream, "{name}: {value}\r\n").unwrap();
                }
                write!(
                    stream,
                    "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });

        let package = resolve(&client().unwrap(), &format!("{base}/Group/Sub/Tool")).unwrap();
        assert_eq!(package.kind, SourceKind::Gitlab);
        assert_eq!(package.id, format!("{}/Group/Sub/Tool:Tool", &base[7..]));
        assert_eq!(package.release_selector.as_deref(), Some("Tool"));
        assert_eq!(package.candidates[0].url, format!("{base}/direct"));
        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
        assert!(requests[1].starts_with(
            "GET /api/v4/projects/Group%2FSub%2FTool/releases/permalink/latest HTTP/1.1"
        ));
    }

    #[test]
    fn probed_gitlab_nested_repository_tag_uses_exact_release_api() {
        let (base, handle) = serve(vec![
            MockResponse {
                status: "500 Internal Server Error",
                headers: vec![("X-GitLab-Meta", "yes")],
                body: String::new(),
            },
            MockResponse {
                status: "200 OK",
                headers: vec![],
                body: format!(
                    r#"{{"tag_name":"v/1","assets":{{"links":[{{"name":"{}","url":"https://example.com/asset","direct_asset_url":null}}]}}}}"#,
                    platform_asset_name()
                ),
            },
        ]);
        let package = resolve_with_preferences(
            &client().unwrap(),
            &format!("{base}/Group/Sub/Tool:v/1"),
            None,
            Channel::Stable,
            None,
        )
        .unwrap();
        assert_eq!(package.id, format!("{}/Group/Sub/Tool", &base[7..]));
        assert_eq!(package.tag.as_deref(), Some("v/1"));
        assert!(package.automatic_pin);
        assert_eq!(package.channel, Channel::Stable);
        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
        assert!(
            requests[1]
                .starts_with("GET /api/v4/projects/Group%2FSub%2FTool/releases/v%2F1 HTTP/1.1")
        );
    }

    #[test]
    fn unknown_host_repository_tag_keeps_direct_url_fallback() {
        let (base, handle) = serve(vec![MockResponse {
            status: "404 Not Found",
            headers: vec![],
            body: String::new(),
        }]);
        let input = format!("{base}/Owner/Tool:release/1.2");
        let package = resolve(&client().unwrap(), &input).unwrap();
        assert_eq!(package.kind, SourceKind::Direct);
        assert_eq!(package.candidates[0].url, input);
        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn direct_release_asset_probes_once_and_skips_discovery() {
        let (base, handle) = serve(vec![MockResponse {
            status: "200 OK",
            headers: vec![],
            body: r#"{"version":"1.24"}"#.into(),
        }]);
        let input = format!(
            "{base}/Owner/Tool/releases/download/v1/{}",
            platform_asset_name()
        );
        let package = resolve(&client().unwrap(), &input).unwrap();
        assert!(package.automatic_pin);
        assert_eq!(package.tag.as_deref(), Some("v1"));
        assert_eq!(package.candidates.len(), 1);
        assert_eq!(package.candidates[0].url, input);
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
    }

    #[test]
    fn gitlab_api_uses_host_derived_private_token() {
        let _guard = TEST_ENV_LOCK.lock().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let release = format!(
            r#"{{"tag_name":"Tool-v1","assets":{{"links":[{{"name":"{}","url":"{base}/asset","direct_asset_url":null}}]}}}}"#,
            platform_asset_name()
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            let read = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{release}",
                release.len()
            )
            .unwrap();
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        // SAFETY: tests touching this process-local variable share TEST_ENV_LOCK.
        unsafe { std::env::set_var("EGET_127_0_0_1_TOKEN", "gitlab-secret") };
        let package = resolve_with_hint(
            &client().unwrap(),
            &format!("{base}/Group/Tool"),
            Some(SourceKind::Gitlab),
        )
        .unwrap();
        // SAFETY: see the serialized set above.
        unsafe { std::env::remove_var("EGET_127_0_0_1_TOKEN") };
        assert_eq!(package.kind, SourceKind::Gitlab);
        let request = handle.join().unwrap().to_ascii_lowercase();
        assert!(request.contains("private-token: gitlab-secret"));
        assert!(!package.source.contains("gitlab-secret"));
    }

    #[test]
    fn cross_origin_redirect_strips_forge_credentials() {
        let _guard = TEST_ENV_LOCK.lock().unwrap();
        let (destination, destination_handle) = serve(vec![MockResponse {
            status: "200 OK",
            headers: vec![],
            body: "payload".into(),
        }]);
        let destination: &'static str = Box::leak(destination.into_boxed_str());
        let (origin, origin_handle) = serve(vec![MockResponse {
            status: "302 Found",
            headers: vec![("Location", destination)],
            body: String::new(),
        }]);
        // SAFETY: this test serializes access to this process-local test variable.
        unsafe { std::env::set_var("EGET_127_0_0_1_TOKEN", "top-secret") };
        let package = ResolvedPackage {
            id: "example/tool".into(),
            owner: "example".into(),
            app: "tool".into(),
            kind: SourceKind::Gitea,
            source: format!("{origin}/Owner/Tool"),
            tag: Some("v1".into()),
            automatic_pin: true,
            pinned: true,
            channel: Channel::Stable,
            release_selector: None,
            forge_origin: Some(format!("{origin}/")),
            candidates: vec![],
        };
        let candidate = AssetCandidate {
            name: "tool".into(),
            url: format!("{origin}/asset"),
        };
        let mut response = asset_response(&client().unwrap(), &package, &candidate).unwrap();
        let mut body = String::new();
        response.read_to_string(&mut body).unwrap();
        // SAFETY: see the serialized set above.
        unsafe { std::env::remove_var("EGET_127_0_0_1_TOKEN") };
        assert_eq!(body, "payload");
        let origin_requests = origin_handle.join().unwrap();
        let destination_requests = destination_handle.join().unwrap();
        assert!(
            origin_requests[0].contains("authorization: token top-secret")
                || origin_requests[0].contains("Authorization: token top-secret")
        );
        assert!(
            !destination_requests[0]
                .to_ascii_lowercase()
                .contains("authorization")
        );
        assert!(
            !destination_requests[0]
                .to_ascii_lowercase()
                .contains("private-token")
        );
    }
}
