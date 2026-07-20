use crate::installer::{InstallOptions, Installer};
use crate::model::RenameRule;
use crate::policy::Channel;
use crate::scope::{Scope, ScopeKind};
use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser, Subcommand};
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "eget",
    version,
    about = "Install standalone executables from GitHub, Gitea, GitLab, and direct URLs"
)]
struct Cli {
    /// Package-management scope
    #[arg(
        long,
        global = true,
        env = "EGET_SCOPE",
        value_name = "system|user|local"
    )]
    scope: Option<ScopeKind>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install one or more packages
    #[command(aliases = ["inst", "i"])]
    Install {
        /// Replace a conflicting command that is not owned by this package
        #[arg(long)]
        force: bool,
        /// Skip packages that are already installed
        #[arg(short = 'p', long)]
        ignore_existing: bool,
        /// Download and install even when the selected version is unchanged
        #[arg(long)]
        reinstall: bool,
        /// Mark the installed package as pinned
        #[arg(long, conflicts_with = "unpin")]
        pin: bool,
        /// Mark the installed package as tracking
        #[arg(long, alias = "no-pin")]
        unpin: bool,
        /// Release channel to track
        #[arg(long, value_name = "stable|prerelease")]
        channel: Option<Channel>,
        /// Directory in which command symlinks are created
        #[arg(long, value_name = "DIRECTORY")]
        to: Option<PathBuf>,
        /// Endpoint that resolves the version used in a {{version}} URL template
        #[arg(long, value_name = "URL")]
        version_url: Option<String>,
        /// Rename a discovered command, expressed as FROM=TO
        #[arg(long = "rename", value_parser = parse_rename, value_name = "FROM=TO")]
        rename_rules: Vec<RenameRule>,
        /// Repository, package ID, or direct URL
        #[arg(required = true)]
        package: Vec<String>,
    },
    /// List installed packages
    #[command(alias = "ls")]
    List {
        /// Package ID prefix or owner by which to filter
        filter: Vec<String>,
    },
    /// Change package tracking policy
    #[command(group(
        ArgGroup::new("policy")
            .required(true)
            .multiple(true)
            .args(["pin", "unpin", "channel"])
    ))]
    Mark {
        #[arg(long, conflicts_with = "unpin")]
        pin: bool,
        #[arg(long, alias = "no-pin")]
        unpin: bool,
        #[arg(long, value_name = "stable|prerelease")]
        channel: Option<Channel>,
        #[arg(required = true, value_name = "PACKAGE_ID")]
        package_id: Vec<String>,
    },
    /// Update selected packages, or every package when no IDs are given
    Update {
        #[arg(short = 'y', long, conflicts_with = "assume_no")]
        assume_yes: bool,
        #[arg(long)]
        assume_no: bool,
        #[arg(value_name = "PACKAGE_ID")]
        package_id: Vec<String>,
    },
    /// Uninstall one or more packages
    #[command(aliases = ["remove", "rm"])]
    Uninstall {
        #[arg(required = true, value_name = "PACKAGE_ID")]
        package_id: Vec<String>,
    },
}

pub fn run(args: Vec<OsString>) -> Result<u8> {
    let cli = Cli::parse_from(normalize(args));
    let destination = match &cli.command {
        Command::Install { to, .. } => to.as_deref().map(absolute_path).transpose()?,
        _ => None,
    };
    let relocate = destination.is_some()
        || std::env::var_os("EGET_BIN_DIR").is_some()
        || std::env::var_os("EGET_BIN").is_some();
    let scope = Scope::detect(cli.scope, destination)?;
    let installer = Installer::new(scope)?;
    match cli.command {
        Command::Install {
            force,
            ignore_existing,
            reinstall,
            pin,
            unpin,
            channel,
            version_url,
            rename_rules,
            package,
            ..
        } => installer.install_many(
            &package,
            &InstallOptions {
                force,
                pin: flag_value(pin, unpin),
                channel,
                reinstall,
                ignore_existing,
                version_url,
                rename_rules,
                relocate,
            },
        ),
        Command::List { filter } => installer.list(&filter),
        Command::Mark {
            pin,
            unpin,
            channel,
            package_id,
        } => installer.mark_many(&package_id, flag_value(pin, unpin), channel),
        Command::Update {
            assume_yes,
            assume_no,
            package_id,
        } => installer.update_many(&package_id, |count| {
            if assume_yes {
                Ok(true)
            } else if assume_no {
                Ok(false)
            } else {
                confirm_updates(count)
            }
        }),
        Command::Uninstall { package_id } => installer.uninstall_many(&package_id),
    }
}

fn normalize(mut arguments: Vec<OsString>) -> Vec<OsString> {
    if arguments.len() == 1 {
        arguments.push("--help".into());
        return arguments;
    }
    let mut index = 1;
    while index < arguments.len() {
        let value = arguments[index].to_string_lossy();
        if value == "--scope" {
            index += 2;
            continue;
        }
        if value.starts_with("--scope=") {
            index += 1;
            continue;
        }
        if matches!(value.as_ref(), "--help" | "-h" | "--version" | "-V") {
            return arguments;
        }
        if matches!(
            value.as_ref(),
            "install"
                | "inst"
                | "i"
                | "list"
                | "ls"
                | "mark"
                | "update"
                | "uninstall"
                | "remove"
                | "rm"
                | "help"
        ) {
            return arguments;
        }
        arguments.insert(index, "install".into());
        return arguments;
    }
    arguments
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("--to must name a directory")
    }
    std::path::absolute(path).with_context(|| format!("resolve {}", path.display()))
}

fn flag_value(positive: bool, negative: bool) -> Option<bool> {
    if positive {
        Some(true)
    } else if negative {
        Some(false)
    } else {
        None
    }
}

fn parse_rename(value: &str) -> Result<RenameRule, String> {
    let (from, to) = value
        .split_once('=')
        .ok_or_else(|| "rename rule must use FROM=TO".to_owned())?;
    if from.is_empty() || to.is_empty() || from.contains('/') || to.contains('/') {
        return Err("rename rule names must be non-empty file names".into());
    }
    Ok(RenameRule(from.into(), to.into()))
}

fn confirm_updates(package_count: usize) -> Result<bool> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    confirm(&mut input, &mut output, package_count)
}

fn confirm(input: &mut impl BufRead, output: &mut impl Write, count: usize) -> Result<bool> {
    let mut answer = String::new();
    loop {
        write!(
            output,
            "Update {count} {}? [yes/no] ",
            if count == 1 { "package" } else { "packages" }
        )?;
        output.flush()?;
        answer.clear();
        if input.read_line(&mut answer)? == 0 {
            writeln!(output)?;
            return Ok(false);
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "yes" | "y" => return Ok(true),
            "no" | "n" | "" => return Ok(false),
            _ => writeln!(output, "Please enter yes or no.")?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_install_follows_global_scope() {
        let arguments = normalize(vec![
            "eget".into(),
            "--scope".into(),
            "user".into(),
            "owner/repo".into(),
        ]);
        assert_eq!(arguments[3], "install");
    }

    #[test]
    fn confirmation_reprompts() {
        let mut input = "maybe\ny\n".as_bytes();
        let mut output = Vec::new();
        assert!(confirm(&mut input, &mut output, 2).unwrap());
    }

    #[test]
    fn command_aliases_are_not_rewritten_as_implicit_installs() {
        for alias in ["inst", "i", "ls", "remove", "rm"] {
            let arguments = normalize(vec!["eget".into(), alias.into()]);
            assert_eq!(arguments[1], alias);
        }
        for arguments in [
            vec!["eget", "inst", "owner/repo"],
            vec!["eget", "i", "owner/repo"],
            vec!["eget", "ls"],
            vec!["eget", "remove", "github.com/owner/repo"],
            vec!["eget", "rm", "github.com/owner/repo"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_ok());
        }
    }

    #[test]
    fn compatibility_flags_parse() {
        let install = Cli::try_parse_from(["eget", "install", "-p", "--no-pin", "owner/repo"]);
        assert!(install.is_ok());
        let mark = Cli::try_parse_from(["eget", "mark", "--no-pin", "github.com/owner/repo"]);
        assert!(mark.is_ok());
    }
}
