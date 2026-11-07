//! `[runners.<name>]`: the machines a host may run a node's tools on, each
//! reached as `ssh <alias>` (spec 07 §3.6). `plak runner pair` and `ps`
//! dial them; sessions retain the paired identity, not a mutable alias.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{Entry, Reader};

/// The name the host's own local runner answers to; no table may claim it.
pub const LOCAL_RUNNER_NAME: &str = "local";

/// One `[runners.<name>]` table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerConfig {
    /// The ssh destination the host dials: a `~/.ssh/config` alias or
    /// `[user@]host`.
    pub ssh: String,
    /// Absolute remote stable-install-link executable, never shell source.
    pub command: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::product_config) struct RunnerSection {
    ssh: Entry,
    command: Entry,
}

/// Every `[runners.<name>]` table, validated and located like the tool
/// sections.
pub(in crate::product_config) fn parse(
    text: &str,
    runners: Option<&BTreeMap<String, RunnerSection>>,
) -> Result<BTreeMap<String, RunnerConfig>, String> {
    let mut parsed = BTreeMap::new();
    for (name, section) in runners.into_iter().flatten() {
        let reader = Reader::new(text, &format!("runners.{name}"));
        if !is_runner_name(name) {
            return Err(
                reader.header("is not a valid runner name; a name matches [a-z][a-z0-9-]{0,31}")
            );
        }
        if name == LOCAL_RUNNER_NAME {
            return Err(reader.header("names the host's own runner, which needs no table"));
        }
        let Some(ssh) = reader.string("ssh", &section.ssh)? else {
            return Err(reader.header("needs ssh = \"<alias>\""));
        };
        if !is_ssh_destination(&ssh) {
            return Err(reader.at_entry(
                &section.ssh,
                "ssh must be an alias or [user@]host: 1 to 255 visible bytes, no whitespace, not starting with -",
            ));
        }
        let Some(command) = reader.string("command", &section.command)? else {
            return Err(reader.header("needs command = \"/absolute/stable/link/plak\"; set [runners.<name>] command to the remote stable install link"));
        };
        if !is_runner_command(&command) {
            return Err(reader.at_entry(&section.command, "command must be an absolute executable path using only [A-Za-z0-9/._+-], not shell source"));
        }
        parsed.insert(name.clone(), RunnerConfig { ssh, command });
    }
    Ok(parsed)
}

pub(crate) fn is_runner_command(command: &str) -> bool {
    command.starts_with('/')
        && command.len() <= 4096
        && command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._+-".contains(&byte))
        && !command.split('/').any(|part| matches!(part, "." | ".."))
        && !command.ends_with('/')
}

/// Checks a runner name a command names, as a `[runners.<name>]` table
/// must be named.
///
/// # Errors
/// Refuses anything else, and the host's own `local` runner.
pub fn validate_runner_name(name: &str) -> Result<(), String> {
    if name == LOCAL_RUNNER_NAME {
        return Err("`local` is the host's own runner; name a [runners.<name>] table".to_owned());
    }
    if is_runner_name(name) {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a valid runner name; a name matches [a-z][a-z0-9-]{{0,31}}"
        ))
    }
}

fn is_runner_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 32
        && bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// An ssh destination the host can pass as one argument without it being
/// read as an option.
fn is_ssh_destination(value: &str) -> bool {
    (1..=255).contains(&value.len())
        && !value.starts_with('-')
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}
