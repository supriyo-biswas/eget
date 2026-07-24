use crate::model::{PackageRecord, RenameRule, SourceKind};
use crate::policy::Channel;
use crate::source;
use crate::template::UrlTemplate;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tempfile::NamedTempFile;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EntryOptions {
    pub pin: Option<bool>,
    pub channel: Option<Channel>,
    pub version_url: Option<String>,
    pub rename_rules: Vec<RenameRule>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub line: usize,
    pub input: String,
    pub options: EntryOptions,
}

#[derive(Clone, Debug)]
struct DocumentLine {
    raw: String,
    entry: Option<Entry>,
}

#[derive(Debug)]
pub struct Manifest {
    path: PathBuf,
    lines: Vec<DocumentLine>,
    changed: bool,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("read package manifest {}", path.display()))?;
        let mut lines = Vec::new();
        for (index, raw) in contents.split_inclusive('\n').enumerate() {
            lines.push(parse_document_line(path, index + 1, raw)?);
        }
        if !contents.is_empty() && !contents.ends_with('\n') && lines.is_empty() {
            lines.push(parse_document_line(path, 1, &contents)?);
        }

        validate_duplicates(path, &lines)?;
        Ok(Self {
            path: path.to_owned(),
            lines,
            changed: false,
        })
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.lines.iter().filter_map(|line| line.entry.as_ref())
    }

    pub fn upsert(&mut self, input: &str, package: &PackageRecord) -> Result<()> {
        let rendered = render_entry(input, package);
        if let Some(index) = self.matching_index(package)? {
            self.replace_line(index, rendered);
        } else {
            self.append_line(rendered);
        }
        Ok(())
    }

    pub fn update_existing(&mut self, package: &PackageRecord) -> Result<()> {
        let Some(index) = self.matching_index(package)? else {
            return Ok(());
        };
        let input = self.lines[index].entry.as_ref().unwrap().input.clone();
        self.replace_line(index, render_entry(&input, package));
        Ok(())
    }

    pub fn remove(&mut self, package: &PackageRecord) -> Result<()> {
        let Some(index) = self.matching_index(package)? else {
            return Ok(());
        };
        self.lines.remove(index);
        self.changed = true;
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        if !self.changed {
            return Ok(());
        }
        let parent = self
            .path
            .parent()
            .context("package manifest has no parent directory")?;
        let metadata = fs::metadata(&self.path)
            .with_context(|| format!("read package manifest metadata {}", self.path.display()))?;
        let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
            format!("create temporary package manifest in {}", parent.display())
        })?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(metadata.permissions().mode()))?;
        for line in &self.lines {
            temporary.write_all(line.raw.as_bytes())?;
        }
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("replace package manifest {}", self.path.display()))?;
        self.changed = false;
        Ok(())
    }

    fn matching_index(&self, package: &PackageRecord) -> Result<Option<usize>> {
        let mut found: Option<usize> = None;
        for (index, line) in self.lines.iter().enumerate() {
            let Some(entry) = &line.entry else {
                continue;
            };
            let identity_input = identity_input(
                &entry.input,
                entry.options.version_url.is_some(),
                package.current_version.as_deref(),
            )?;
            let hint = source::package_identity_hint(&identity_input, Some(package.source_kind))
                .with_context(|| {
                    format!(
                        "identify package manifest entry {}:{}",
                        self.path.display(),
                        entry.line
                    )
                })?;
            if hint.ids.iter().any(|id| id == package.id.as_str()) {
                if let Some(previous) = found {
                    bail!(
                        "duplicate package {} in {} on lines {} and {}",
                        package.id,
                        self.path.display(),
                        self.lines[previous].entry.as_ref().unwrap().line,
                        entry.line
                    )
                }
                found = Some(index);
            }
        }
        Ok(found)
    }

    fn replace_line(&mut self, index: usize, rendered: String) {
        let ending = line_ending(&self.lines[index].raw);
        let suffix = preserved_suffix(&self.lines[index].raw);
        let raw = format!("{rendered}{suffix}{ending}");
        if self.lines[index].raw != raw {
            self.lines[index] = DocumentLine {
                raw,
                entry: parse_entry(&rendered, self.lines[index].entry.as_ref().unwrap().line)
                    .ok()
                    .flatten(),
            };
            self.changed = true;
        }
    }

    fn append_line(&mut self, rendered: String) {
        if let Some(last) = self.lines.last_mut()
            && !last.raw.ends_with('\n')
        {
            last.raw.push('\n');
            self.changed = true;
        }
        let line = self.lines.len() + 1;
        self.lines.push(DocumentLine {
            raw: format!("{rendered}\n"),
            entry: parse_entry(&rendered, line).ok().flatten(),
        });
        self.changed = true;
    }
}

fn parse_document_line(path: &Path, line: usize, raw: &str) -> Result<DocumentLine> {
    let body = raw
        .strip_suffix('\n')
        .unwrap_or(raw)
        .strip_suffix('\r')
        .unwrap_or_else(|| raw.strip_suffix('\n').unwrap_or(raw));
    let entry = parse_entry(body, line)
        .with_context(|| format!("parse package manifest {}:{line}", path.display()))?;
    Ok(DocumentLine {
        raw: raw.to_owned(),
        entry,
    })
}

fn parse_entry(text: &str, line: usize) -> Result<Option<Entry>> {
    let words = shlex::split(text).context("malformed shell quoting")?;
    if words.is_empty() {
        return Ok(None);
    }

    let mut input = None;
    let mut options = EntryOptions::default();
    let mut index = 0;
    let mut options_ended = false;
    while index < words.len() {
        let word = &words[index];
        if !options_ended && word == "--" {
            options_ended = true;
            index += 1;
            continue;
        }
        if !options_ended && word.starts_with('-') {
            let (name, inline) = word
                .split_once('=')
                .map_or((word.as_str(), None), |value| (value.0, Some(value.1)));
            match name {
                "--pin" => {
                    require_no_value(name, inline)?;
                    set_pin(&mut options.pin, true)?;
                }
                "--unpin" | "--no-pin" => {
                    require_no_value(name, inline)?;
                    set_pin(&mut options.pin, false)?;
                }
                "--channel" => {
                    if options.channel.is_some() {
                        bail!("--channel may only be specified once")
                    }
                    let value = option_value(name, inline, &words, &mut index)?;
                    options.channel = Some(Channel::from_str(value)?);
                }
                "--version-url" => {
                    if options.version_url.is_some() {
                        bail!("--version-url may only be specified once")
                    }
                    options.version_url =
                        Some(option_value(name, inline, &words, &mut index)?.to_owned());
                }
                "--rename" => {
                    let value = option_value(name, inline, &words, &mut index)?;
                    options.rename_rules.push(parse_rename(value)?);
                }
                "--force" | "--reinstall" | "--ignore-existing" | "-p" | "--to" => {
                    bail!("{name} is a run-only option and is not allowed in the package manifest")
                }
                _ => bail!("unknown package manifest option {name:?}"),
            }
        } else if input.replace(word.clone()).is_some() {
            bail!("each package manifest line must contain exactly one package location")
        }
        index += 1;
    }

    let input = input.context("package manifest line does not contain a package location")?;
    UrlTemplate::parse(&input, options.version_url.is_some())?;
    Ok(Some(Entry {
        line,
        input,
        options,
    }))
}

fn option_value<'a>(
    name: &str,
    inline: Option<&'a str>,
    words: &'a [String],
    index: &mut usize,
) -> Result<&'a str> {
    if let Some(value) = inline {
        if value.is_empty() {
            bail!("{name} requires a value")
        }
        return Ok(value);
    }
    *index += 1;
    words
        .get(*index)
        .map(String::as_str)
        .with_context(|| format!("{name} requires a value"))
}

fn require_no_value(name: &str, value: Option<&str>) -> Result<()> {
    if value.is_some() {
        bail!("{name} does not accept a value")
    }
    Ok(())
}

fn set_pin(pin: &mut Option<bool>, value: bool) -> Result<()> {
    if pin.replace(value).is_some() {
        bail!("pinning policy may only be specified once")
    }
    Ok(())
}

fn parse_rename(value: &str) -> Result<RenameRule> {
    let (from, to) = value
        .split_once('=')
        .context("rename rule must use FROM=TO")?;
    if from.is_empty() || to.is_empty() || from.contains('/') || to.contains('/') {
        bail!("rename rule names must be non-empty file names")
    }
    Ok(RenameRule(from.into(), to.into()))
}

fn validate_duplicates(path: &Path, lines: &[DocumentLine]) -> Result<()> {
    let mut identities: Vec<(usize, BTreeSet<String>)> = Vec::new();
    for line in lines {
        let Some(entry) = &line.entry else {
            continue;
        };
        let identity_input =
            identity_input(&entry.input, entry.options.version_url.is_some(), None)?;
        let ids = source::package_identity_hint(&identity_input, None)
            .with_context(|| {
                format!(
                    "identify package manifest entry {}:{}",
                    path.display(),
                    entry.line
                )
            })?
            .ids
            .into_iter()
            .collect::<BTreeSet<_>>();
        if let Some((other_line, _)) = identities
            .iter()
            .find(|(_, other)| !other.is_disjoint(&ids))
        {
            bail!(
                "duplicate package entries in {} on lines {} and {}",
                path.display(),
                other_line,
                entry.line
            )
        }
        identities.push((entry.line, ids));
    }
    Ok(())
}

fn identity_input(input: &str, has_version_url: bool, version: Option<&str>) -> Result<String> {
    let template = UrlTemplate::parse(input, has_version_url)?;
    let version = template
        .needs_version()
        .then_some(version.unwrap_or("0.0.0"));
    template.render_current(version)
}

fn render_entry(input: &str, package: &PackageRecord) -> String {
    let source = match package.source_kind {
        SourceKind::Direct => input.to_owned(),
        SourceKind::Github | SourceKind::Gitlab | SourceKind::Gitea => {
            if package.pinned
                && let Some(tag) = &package.current_version
            {
                let selector_suffix = package
                    .release_selector
                    .as_ref()
                    .map(|selector| format!(":{selector}"));
                let base = selector_suffix
                    .as_deref()
                    .and_then(|suffix| package.id.as_str().strip_suffix(suffix))
                    .unwrap_or(package.id.as_str());
                format!("{base}:{tag}")
            } else {
                package.id.to_string()
            }
        }
    };

    let mut words = vec![source];
    if !package.pinned {
        words.push("--no-pin".into());
    }
    if package.channel == Some(Channel::Prerelease) {
        words.extend(["--channel".into(), "prerelease".into()]);
    }
    if let Some(url) = &package.version_check_url {
        words.extend(["--version-url".into(), url.clone()]);
    }
    for RenameRule(from, to) in &package.rename_rules {
        words.extend(["--rename".into(), format!("{from}={to}")]);
    }
    words
        .iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./{}~-".contains(&byte))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

fn line_ending(raw: &str) -> &'static str {
    if raw.ends_with("\r\n") {
        "\r\n"
    } else if raw.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

fn preserved_suffix(raw: &str) -> &str {
    let body = raw
        .strip_suffix('\n')
        .unwrap_or(raw)
        .strip_suffix('\r')
        .unwrap_or_else(|| raw.strip_suffix('\n').unwrap_or(raw));
    let mut quote = None;
    let mut escaped = false;
    let mut word_start = true;
    for (index, character) in body.char_indices() {
        if escaped {
            escaped = false;
            word_start = false;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (None, '\'' | '"') => {
                quote = Some(character);
                word_start = false;
            }
            (Some('\''), _) => {}
            (_, '\\') => {
                escaped = true;
                word_start = false;
            }
            (None, '#') if word_start => {
                let start = body[..index].trim_end_matches(char::is_whitespace).len();
                return &body[start..];
            }
            (None, character) => word_start = character.is_whitespace(),
            _ => {}
        }
    }
    let start = body.trim_end_matches(char::is_whitespace).len();
    &body[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HttpValidators, PackageId};

    fn load(contents: &str) -> Result<Manifest> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("eget-packages.txt");
        fs::write(&path, contents).unwrap();
        Manifest::load(&path)
    }

    #[test]
    fn parses_shell_words_comments_and_durable_options() {
        let manifest = load(
            "# tools\nBurntSushi/ripgrep:1.9\n\
             'https://example.com/tool-{{version}}.tar.gz' --version-url \
             'https://example.com/latest version.txt' --rename 'tool=other tool' # note\n\
             \"https://example.com/other-{{kernel}}-{% if arch == 'x86_64' %}amd64{% else %}arm64{% endif %}.tar.gz\"\n",
        )
        .unwrap();
        let entries = manifest.entries().collect::<Vec<_>>();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].input, "BurntSushi/ripgrep:1.9");
        assert_eq!(
            entries[1].options.version_url.as_deref(),
            Some("https://example.com/latest version.txt")
        );
        assert_eq!(
            entries[1].options.rename_rules,
            [RenameRule("tool".into(), "other tool".into())]
        );
        assert_eq!(
            entries[2].input,
            "https://example.com/other-{{kernel}}-{% if arch == 'x86_64' %}amd64{% else %}arm64{% endif %}.tar.gz"
        );
    }

    #[test]
    fn rejects_run_only_options_and_duplicate_packages() {
        assert!(
            load("owner/repo --force\n")
                .unwrap_err()
                .to_string()
                .contains("parse package manifest")
        );
        assert!(
            load("owner/repo\ngithub.com/owner/repo --no-pin\n")
                .unwrap_err()
                .to_string()
                .contains("duplicate package entries")
        );
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("owner/repo:1.0"), "owner/repo:1.0");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn forge_entries_use_canonical_ids_and_exact_tags() {
        let mut package = PackageRecord {
            id: PackageId::parse("github.com/owner/repo:tool").unwrap(),
            current_version: Some("tool/v1.9".into()),
            owner: "github.com/owner".into(),
            app: "repo".into(),
            source_kind: SourceKind::Github,
            installation_dir: "/tmp/package".into(),
            bin_dir: "/tmp/bin".into(),
            pinned: true,
            installed_asset_url: "https://example.com/tool".into(),
            channel: Some(Channel::Prerelease),
            release_selector: Some("tool".into()),
            version_check_url: None,
            validators: HttpValidators::default(),
            rename_rules: vec![RenameRule("tool".into(), "other tool".into())],
            installed_at: "now".into(),
            updated_at: None,
            binaries: vec!["tool".into()],
        };
        let rendered = render_entry("owner/repo", &package);
        assert_eq!(
            rendered,
            "github.com/owner/repo:tool/v1.9 --channel prerelease --rename 'tool=other tool'"
        );
        let locref = shlex::split(&rendered).unwrap().remove(0);
        assert!(
            source::package_identity_hint(&locref, Some(SourceKind::Github))
                .unwrap()
                .ids
                .contains(&package.id.to_string())
        );

        package.pinned = false;
        assert_eq!(
            render_entry("owner/repo", &package),
            "github.com/owner/repo:tool --no-pin --channel prerelease --rename 'tool=other tool'"
        );
    }

    #[test]
    fn replacing_an_entry_preserves_inline_comments_and_other_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("eget-packages.txt");
        fs::write(&path, "# tools\nowner/repo   # important\n\n").unwrap();
        let mut manifest = Manifest::load(&path).unwrap();
        let package = PackageRecord {
            id: PackageId::parse("github.com/owner/repo").unwrap(),
            current_version: Some("v1.0".into()),
            owner: "github.com/owner".into(),
            app: "repo".into(),
            source_kind: SourceKind::Github,
            installation_dir: "/tmp/package".into(),
            bin_dir: "/tmp/bin".into(),
            pinned: true,
            installed_asset_url: "https://example.com/tool".into(),
            channel: Some(Channel::Stable),
            release_selector: None,
            version_check_url: None,
            validators: HttpValidators::default(),
            rename_rules: Vec::new(),
            installed_at: "now".into(),
            updated_at: None,
            binaries: vec!["tool".into()],
        };
        manifest.upsert("owner/repo", &package).unwrap();
        manifest.save().unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "# tools\ngithub.com/owner/repo:v1.0   # important\n\n"
        );
    }
}
