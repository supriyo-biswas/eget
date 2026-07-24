use crate::compat::{Host, HostArch, HostOs};
use anyhow::{Context, Result, bail};
use minijinja::{Environment, Error, ErrorKind, UndefinedBehavior, context, escape_formatter};
use std::collections::BTreeSet;

const ALLOWED_VARIABLES: [&str; 3] = ["arch", "kernel", "version"];

#[derive(Debug)]
pub struct UrlTemplate<'a> {
    source: &'a str,
    variables: BTreeSet<String>,
    dynamic: bool,
}

impl<'a> UrlTemplate<'a> {
    pub fn parse(source: &'a str, has_version_url: bool) -> Result<Self> {
        validate_statements(source)?;
        let environment = environment();
        let template = environment
            .template_from_str(source)
            .context("parse URL template")?;
        let variables = template
            .undeclared_variables(false)
            .into_iter()
            .collect::<BTreeSet<_>>();
        if let Some(variable) = variables
            .iter()
            .find(|variable| !ALLOWED_VARIABLES.contains(&variable.as_str()))
        {
            bail!("URL template uses unsupported variable {variable:?}")
        }
        let needs_version = variables.contains("version");
        if has_version_url && !needs_version {
            bail!("--version-url requires the package URL template to reference version")
        }
        if needs_version && !has_version_url {
            bail!("URL template references version but --version-url was not provided")
        }
        Ok(Self {
            source,
            variables,
            dynamic: source.contains("{{") || source.contains("{%"),
        })
    }

    pub fn is_dynamic(&self) -> bool {
        self.dynamic
    }

    pub fn needs_version(&self) -> bool {
        self.variables.contains("version")
    }

    pub fn render_current(&self, version: Option<&str>) -> Result<String> {
        if !self.dynamic {
            return Ok(self.source.to_owned());
        }
        self.render(Host::current()?, version)
    }

    pub fn render(&self, host: Host, version: Option<&str>) -> Result<String> {
        if !self.dynamic {
            return Ok(self.source.to_owned());
        }
        if self.needs_version() && version.is_none() {
            bail!("URL template requires a resolved version")
        }
        environment()
            .render_str(
                self.source,
                context! {
                    kernel => kernel(host.os),
                    arch => arch(host.arch),
                    version => version,
                },
            )
            .context("render URL template")
    }
}

pub fn is_dynamic(source: &str) -> bool {
    source.contains("{{") || source.contains("{%")
}

fn environment() -> Environment<'static> {
    let mut environment = Environment::new();
    environment.set_undefined_behavior(UndefinedBehavior::Strict);
    environment.set_formatter(|output, state, value| {
        if value.is_undefined() || value.is_none() {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                "URL template expression did not return a value",
            ));
        }
        let Some(value) = value.as_str() else {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                "URL template expressions must return strings",
            ));
        };
        if value.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                "URL template expression returned an empty string",
            ));
        }
        escape_formatter(output, state, &value.into())
    });
    environment
}

fn kernel(os: HostOs) -> &'static str {
    match os {
        HostOs::Linux => "linux",
        HostOs::Macos => "darwin",
    }
}

fn arch(arch: HostArch) -> &'static str {
    match arch {
        HostArch::X86_64 => "x86_64",
        HostArch::Aarch64 => "aarch64",
    }
}

fn validate_statements(source: &str) -> Result<()> {
    let mut offset = 0;
    while let Some((start, delimiter)) = next_delimiter(source, offset) {
        match delimiter {
            "{{" => {
                offset =
                    tag_end(source, start + 2, "}}").context("unclosed URL template expression")?;
            }
            "{%" => {
                let end =
                    tag_end(source, start + 2, "%}").context("unclosed URL template statement")?;
                let body = source[start + 2..end - 2]
                    .trim()
                    .trim_start_matches('-')
                    .trim();
                let keyword = body.split_whitespace().next().unwrap_or_default();
                if !matches!(keyword, "if" | "elif" | "else" | "endif") {
                    bail!("URL templates do not support the statement {keyword:?}")
                }
                offset = end;
            }
            "{#" => bail!("URL templates do not support comments"),
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn next_delimiter(source: &str, offset: usize) -> Option<(usize, &'static str)> {
    ["{{", "{%", "{#"]
        .into_iter()
        .filter_map(|delimiter| {
            source[offset..]
                .find(delimiter)
                .map(|index| (offset + index, delimiter))
        })
        .min_by_key(|(index, _)| *index)
}

fn tag_end(source: &str, offset: usize, delimiter: &str) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    for (relative, character) in source[offset..].char_indices() {
        let index = offset + relative;
        if escaped {
            escaped = false;
            continue;
        }
        if quote.is_some() && character == '\\' {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if quote.is_none() && source[index..].starts_with(delimiter) {
            return Some(index + delimiter.len());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX_X86_64: Host = Host {
        os: HostOs::Linux,
        arch: HostArch::X86_64,
    };
    const LINUX_AARCH64: Host = Host {
        os: HostOs::Linux,
        arch: HostArch::Aarch64,
    };
    const MACOS_AARCH64: Host = Host {
        os: HostOs::Macos,
        arch: HostArch::Aarch64,
    };

    #[test]
    fn renders_compact_variables_and_conditional_statements() {
        let template = UrlTemplate::parse(
            "https://example.com/tool-{{kernel}}-{% if arch == 'x86_64' %}amd64{% else %}arm64{% endif %}",
            false,
        )
        .unwrap();
        assert_eq!(
            template.render(LINUX_X86_64, None).unwrap(),
            "https://example.com/tool-linux-amd64"
        );
        assert_eq!(
            template.render(MACOS_AARCH64, None).unwrap(),
            "https://example.com/tool-darwin-arm64"
        );
    }

    #[test]
    fn renders_nested_inline_conditionals() {
        let template = UrlTemplate::parse(
            "https://example.com/{{ 'amd64' if arch == 'x86_64' else ('arm64' if kernel == 'linux' else 'darwin-arm64') }}",
            false,
        )
        .unwrap();
        assert_eq!(
            template.render(LINUX_X86_64, None).unwrap(),
            "https://example.com/amd64"
        );
        assert_eq!(
            template.render(LINUX_AARCH64, None).unwrap(),
            "https://example.com/arm64"
        );
        assert_eq!(
            template.render(MACOS_AARCH64, None).unwrap(),
            "https://example.com/darwin-arm64"
        );
    }

    #[test]
    fn version_reference_and_option_must_match() {
        let template = UrlTemplate::parse("https://example.com/{{version}}", true).unwrap();
        assert_eq!(
            template.render(LINUX_X86_64, Some("v1.2.3")).unwrap(),
            "https://example.com/v1.2.3"
        );
        assert!(
            UrlTemplate::parse("https://example.com/{{ version }}", false)
                .unwrap_err()
                .to_string()
                .contains("--version-url was not provided")
        );
        assert!(
            UrlTemplate::parse("https://example.com/tool", true)
                .unwrap_err()
                .to_string()
                .contains("requires the package URL template to reference version")
        );
    }

    #[test]
    fn rejects_unsupported_variables_statements_and_comments() {
        assert!(UrlTemplate::parse("https://example.com/{{ os }}", false).is_err());
        assert!(
            UrlTemplate::parse(
                "https://example.com/{% for item in items %}{{ item }}{% endfor %}",
                false
            )
            .is_err()
        );
        assert!(UrlTemplate::parse("https://example.com/{# comment #}", false).is_err());
    }

    #[test]
    fn rejects_empty_missing_and_non_string_expression_values() {
        for source in [
            "https://example.com/{{ '' }}",
            "https://example.com/{{ 'linux' if false }}",
            "https://example.com/{{ none }}",
            "https://example.com/{{ arch == 'x86_64' }}",
        ] {
            let template = UrlTemplate::parse(source, false).unwrap();
            assert!(template.render(LINUX_X86_64, None).is_err(), "{source}");
        }
    }

    #[test]
    fn delimiters_inside_strings_are_not_treated_as_tags() {
        let template = UrlTemplate::parse(
            r#"https://example.com/{{ "{% not-a-statement %}" }}"#,
            false,
        )
        .unwrap();
        assert_eq!(
            template.render(LINUX_X86_64, None).unwrap(),
            "https://example.com/{% not-a-statement %}"
        );
    }
}
