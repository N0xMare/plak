//! `[runners.<name>]`: the machines a host may run a node's tools on, each
//! reached as `ssh <alias>` (spec 07 §3.6). Parsed and validated only; no
//! runner is dialed before the runner steps land.

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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::product_config) struct RunnerSection {
    ssh: Entry,
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
        parsed.insert(name.clone(), RunnerConfig { ssh });
    }
    Ok(parsed)
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
