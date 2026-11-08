//! Product command grammar for transient typed refusals.

use crate::notice_text::{Frontend, RemedyContext, with_remedy};
use plakous_host_protocol::refusal::RefusalAdvice;

pub(crate) fn child_progress(
    mut progress: plakous_host_protocol::views::ChildProgress,
) -> plakous_host_protocol::views::ChildProgress {
    if let Some(account) = &progress.credential_wait {
        progress.activity = Some(format!("waiting for login: run `plak login {account}`"));
    }
    progress
}

pub(crate) fn node_fault(node: &plakous_host_protocol::views::NodeView) -> Option<String> {
    node.fault.as_ref().map(|fault| {
        if node.recovery_abandoned {
            format!("{fault}; operator action: session delete --abandon-children")
        } else {
            fault.clone()
        }
    })
}

pub(crate) fn advice_text(advice: &RefusalAdvice, frontend: Frontend) -> String {
    match advice {
        RefusalAdvice::CheckStorage => "check `plak doctor`".into(),
        RefusalAdvice::ChildCredential { account, .. } => format!("choose another template, or ask the operator to run `plak login {}` in a shell", crate::notice_text::account_label(*account)),
        RefusalAdvice::UnknownModel { account, .. } => format!("run `plak models {account}` to list the models it offers"),
        RefusalAdvice::ExplicitEffort { offered, .. } => match frontend {
            Frontend::Tui => format!("choose an explicit root effort with /agents from {}", offered.join("|")),
            _ => format!("requires --effort {}", offered.join("|")),
        },
        RefusalAdvice::EffortDowngraded => match frontend {
            Frontend::Tui => "choose a profile with an admitted explicit root effort through /agents; refusing to run".into(),
            _ => "requested --effort was downgraded by the harness plan; refuse to run".into(),
        },
        RefusalAdvice::McpLogin { server, scopes } => if scopes.is_empty() {
            format!("run `plak mcp login {server}`")
        } else {
            format!("run `plak mcp login {server} --scope '{}'`", scopes.join(" ").replace('\'', r"'\''"))
        },
        RefusalAdvice::SecretMissing { name, .. } => format!("run `plak secret set {name}`"),
        RefusalAdvice::ExportSession { session_id } => format!("recover with the writing build's `plak session export {session_id}` then this build's `plak session import <file>`"),
    }
}

/// Read only the topology tool's structured error payload, never rendered prose.
pub(crate) fn tool_error_text(name: &str, raw: &str, context: RemedyContext<'_>) -> Option<String> {
    use plakous_host_protocol::refusal::ToolRefusal;
    use plakous_host_protocol::views::{ActivityKind, ToolOperation as O};
    let operation = O::classify(name, ActivityKind::Tool, None);
    if operation == O::Mcp {
        let parsed = plakous_host_protocol::views::presentation::McpCall::parse(name)?;
        return match ToolRefusal::decode(raw)? {
            ToolRefusal::McpLogin {
                message,
                server,
                scope,
            } if parsed.server == server => Some(format!(
                "{}; {}",
                message,
                advice_text(
                    &RefusalAdvice::McpLogin {
                        server,
                        scopes: scope.split_ascii_whitespace().map(str::to_owned).collect(),
                    },
                    context.frontend
                )
            )),
            ToolRefusal::McpSecret {
                message,
                server,
                name,
            } if parsed.server == server => Some(format!(
                "{}; {}",
                message,
                advice_text(
                    &RefusalAdvice::SecretMissing { server, name },
                    context.frontend
                )
            )),
            ToolRefusal::McpLogin { .. }
            | ToolRefusal::McpSecret { .. }
            | ToolRefusal::Topology { .. } => None,
        };
    }
    if !matches!(
        operation,
        O::SpawnAgent
            | O::WaitAgent
            | O::ListAgents
            | O::MessageAgent
            | O::Adopt
            | O::CloseAgent
            | O::InterruptAgent
            | O::RecoverAgent
            | O::DiscardWorkspace
            | O::StopAll
            | O::SubmitResult
    ) {
        return None;
    }
    let ToolRefusal::Topology {
        message,
        advice,
        remedy,
        ..
    } = ToolRefusal::decode(raw)?
    else {
        return None;
    };
    if remedy.is_none() && advice.is_none() {
        return None;
    }
    let frontend = context.frontend;
    let mut text = with_remedy(&message, remedy.as_ref(), context);
    if let Some(advice) = advice {
        text.push_str("; ");
        text.push_str(&advice_text(&advice, frontend));
    }
    Some(text)
}

pub(crate) fn failure_text(failure: &crate::CliFailure, context: RemedyContext<'_>) -> String {
    let frontend = context.frontend;
    if let Some(RefusalAdvice::ExplicitEffort {
        account,
        model,
        offered,
    }) = failure.advice()
    {
        if frontend != Frontend::Tui {
            return format!(
                "{account} model `{model}` requires --effort {}",
                offered.join("|")
            );
        }
    }
    let mut text = with_remedy(failure.message(), failure.remedy(), context);
    if let Some(advice) = failure.advice() {
        text.push_str("; ");
        text.push_str(&advice_text(advice, frontend));
    }
    text
}

pub(crate) fn host_failure_text(
    failure: &plakous_host_protocol::status::HostFailure,
    context: RemedyContext<'_>,
) -> String {
    let frontend = context.frontend;
    let mut text = with_remedy(
        failure.diagnostic.as_str(),
        failure.remedy.as_ref(),
        context,
    );
    if let Some(advice) = &failure.advice {
        text.push_str("; ");
        text.push_str(&advice_text(advice, frontend));
    }
    text
}

#[cfg(test)]
mod tests;
