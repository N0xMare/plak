use std::io;

use crate::home::paths::PlakPaths;
use crate::{CliResult, ProcessOutcome};

use super::McpCommand;

pub(crate) fn run(
    _paths: &PlakPaths,
    _command: McpCommand,
    _stdin: &mut dyn io::BufRead,
    _stdout: &mut dyn io::Write,
    _stderr: &mut dyn io::Write,
    _ui: &mut impl crate::commands::login_flow::LoginUi,
) -> CliResult<ProcessOutcome> {
    Err(plakous_hosted::capabilities::CapabilityUnavailable {
        id: "mcp",
        kind: plakous_hosted::capabilities::CapabilityKind::ToolPack,
        feature: "mcp",
    }
    .into())
}

pub(crate) fn session_overview(
    _paths: &PlakPaths,
    _session: Option<&plakous_hosted::runtime::SurfaceSession>,
) -> String {
    plakous_hosted::capabilities::CapabilityUnavailable {
        id: "mcp",
        kind: plakous_hosted::capabilities::CapabilityKind::ToolPack,
        feature: "mcp",
    }
    .to_string()
}
