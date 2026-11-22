//! Detached exact-reopen verification.

use super::{CliResult, HostConfig, startup};

/// What an exact reopen of a session's head would do on this build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReopenVerdict {
    /// The head opens exactly.
    Opens,
    /// The same branch advanced even after following its head once.
    HeadAdvanced(String),
    /// The open refuses because this build derives a different frozen binding
    /// input; only a carry into a new session continues the conversation.
    BindingChanged {
        /// The input that differs.
        change: plakous::agent::BindingChange,
        /// This build's route adapter revision, reported for a capability
        /// binding that differs by digest alone (the recorded side keeps only
        /// the digest).
        current_revision: Option<String>,
    },
    /// The open refuses for another reason (credentials, configuration,
    /// storage), with the refusal's text.
    Refused(String),
    /// The open cannot be verified without an effect a verification never
    /// takes (listing MCP servers whose tools were never frozen), or the head
    /// holds unsettled work its owner settles first
    /// ([`REOPEN_UNSETTLED_WORK`]), with the reason.
    Unknown(String),
}

/// The [`ReopenVerdict::Unknown`] reason for a head whose owner is still
/// running a turn: only an owning open settles it, so nothing past the head
/// was compared. A verification after the owner released it answers.
pub const REOPEN_UNSETTLED_WORK: &str = "unsettled_work";

/// Answers whether an exact reopen of `head` would attach, by running the open's
/// own authority check, runtime compilation and Plakous verification without
/// taking the branch lease, attaching, recovering or reattaching children. An
/// agent tree compiles over a detached copy, so nothing persisted changes. The
/// caller keeps credential renewal out of it: a native route whose access
/// token is fresh resolves without renewing.
///
/// # Errors
/// Returns an executor failure; every open refusal is a verdict.
pub fn verify_exact_reopen<P: plakous_host_protocol::ProtocolExtension>(
    mut config: HostConfig<P>,
    head: plakous::sessions::SessionLocator,
) -> CliResult<ReopenVerdict> {
    let tools = startup::root_tool_selection(&config.session);
    if tools.selection.mcp_servers().next().is_some() && tools.mcp_tools.is_none() {
        return Ok(ReopenVerdict::Unknown("mcp_tools_unfrozen".to_owned()));
    }
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|error| crate::CliFailure::new(error.to_string()))?;
    let factory = Arc::clone(&config.runtime_factory);
    let mut stderr = Vec::new();
    let mut checked = head;
    let mut verified = executor
        .block_on(factory.verify_head(&mut config, head, &mut stderr))
        .map_err(|error| crate::CliFailure::new(error.to_string()))?;
    // A caller's head may trail the runtime by projection; the open adopts
    // the current head once, and so does its verification.
    if let Err(failure) = &verified
        && let Some(current) = failure.current_locator
        && same_branch_advanced(checked, current)
    {
        checked = current;
        verified = executor
            .block_on(factory.verify_head(&mut config, current, &mut stderr))
            .map_err(|error| crate::CliFailure::new(error.to_string()))?;
    }
    Ok(match verified {
        Ok(()) => ReopenVerdict::Opens,
        Err(failure)
            if failure
                .current_locator
                .is_some_and(|current| same_branch_advanced(checked, current)) =>
        {
            ReopenVerdict::HeadAdvanced(failure.failure.message().to_owned())
        }
        Err(failure) if failure.unsettled_work => {
            ReopenVerdict::Unknown(REOPEN_UNSETTLED_WORK.to_owned())
        }
        Err(failure) => match failure.binding_change {
            Some(change) => ReopenVerdict::BindingChanged {
                current_revision: matches!(
                    *change,
                    plakous::agent::BindingChange::ModelCapabilities
                )
                .then(|| config.target.native_resolved())
                .flatten()
                .map(|resolved| resolved.adapter_revision().to_owned()),
                change: *change,
            },
            None => ReopenVerdict::Refused(failure.failure.message().to_owned()),
        },
    })
}

fn same_branch_advanced(
    checked: plakous::sessions::SessionLocator,
    current: plakous::sessions::SessionLocator,
) -> bool {
    current != checked
        && current.session_id() == checked.session_id()
        && current.branch_id() == checked.branch_id()
}
