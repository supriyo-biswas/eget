use crate::policy::Channel;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    Github,
    Gitlab,
    Gitea,
    Direct,
}

impl SourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Gitea => "gitea",
            Self::Direct => "direct",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        value.parse().ok()
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SourceKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "github" => Ok(Self::Github),
            "gitlab" => Ok(Self::Gitlab),
            "gitea" => Ok(Self::Gitea),
            "direct" => Ok(Self::Direct),
            _ => bail!("unknown package source kind {value:?}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageId(String);

impl PackageId {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let (owner, app) = split_package_id(&value)?;
        if owner.is_empty() || app.is_empty() {
            bail!("invalid package ID {value:?}")
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn parts(&self) -> Result<(&str, &str)> {
        split_package_id(&self.0)
    }

    pub fn directory_name(&self) -> String {
        data_encoding::BASE32_NOPAD
            .encode(&xxhash_rust::xxh3::xxh3_128(self.0.as_bytes()).to_be_bytes())
            .to_ascii_lowercase()
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn split_package_id(id: &str) -> Result<(&str, &str)> {
    let without_selector =
        id.rsplit_once(':').map_or(
            id,
            |(base, selector)| {
                if selector.contains('/') { id } else { base }
            },
        );
    let (owner, app) = without_selector
        .rsplit_once('/')
        .context("package ID must contain a source and application")?;
    Ok((owner, app))
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RenameRule(pub String, pub String);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HttpValidators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageRecord {
    pub id: PackageId,
    pub current_version: Option<String>,
    pub owner: String,
    pub app: String,
    pub source_kind: SourceKind,
    pub installation_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub pinned: bool,
    pub installed_asset_url: String,
    pub channel: Option<Channel>,
    pub release_selector: Option<String>,
    pub version_check_url: Option<String>,
    pub validators: HttpValidators,
    pub rename_rules: Vec<RenameRule>,
    pub installed_at: String,
    pub updated_at: Option<String>,
    pub binaries: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeKind {
    Gitea,
    Gitlab,
    Unknown,
}

impl ProbeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gitea => "gitea",
            Self::Gitlab => "gitlab",
            Self::Unknown => "unknown",
        }
    }
}

impl FromStr for ProbeKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "gitea" => Ok(Self::Gitea),
            "gitlab" => Ok(Self::Gitlab),
            "unknown" => Ok(Self::Unknown),
            _ => bail!("unknown source probe kind {value:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_ids_split_selectors_without_confusing_ports() {
        let selected = PackageId::parse("gitlab.example:8443/group/tool:v2").unwrap();
        assert_eq!(
            selected.parts().unwrap(),
            ("gitlab.example:8443/group", "tool")
        );
        assert_eq!(selected.directory_name().len(), 26);

        let ipv6 = PackageId::parse("[2a00:1abc:3df::e01]/tool").unwrap();
        assert_eq!(ipv6.parts().unwrap(), ("[2a00:1abc:3df::e01]", "tool"));
    }
}
