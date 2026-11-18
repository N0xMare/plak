//! Frontend-specific operator help selected only from semantic remedy codes.

use plakous_host_protocol::notice_remedy::{AccountTarget, CredentialCondition, NoticeRemedy};

#[cfg(test)]
pub(crate) mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Frontend {
    Wire,
    Run,
    Jsonl,
}

pub(crate) struct RemedyContext<'a> {
    pub(crate) frontend: Frontend,
    pub(crate) session_id: Option<&'a str>,
}

pub(crate) fn restored_pause(
    pause: plakous_host_protocol::status::HostPause,
    context: RemedyContext<'_>,
) -> String {
    let mut text = String::new();
    if pause.queue {
        text.push_str("the queue is paused and will not auto-drain; ");
        text.push_str(&match context.frontend {
            Frontend::Wire => {
                "send resume_queue to resume delivery or select and send one queued item through the client".to_owned()
            }
            Frontend::Run => format!(
                "reopen `plakd agent --session {}` and send resume_queue; selecting and sending one queued item remains a client action",
                context.session_id.unwrap_or("<session>")
            ),
            Frontend::Jsonl => {
                "send resume_queue to drain; selecting and sending one item remains a client action"
                    .to_owned()
            }
        });
    }
    text
}

pub(crate) fn account_label(account: AccountTarget) -> String {
    match account {
        AccountTarget::Codex => "codex".to_owned(),
        AccountTarget::Grok => "grok".to_owned(),
        AccountTarget::Claude => "claude".to_owned(),
        AccountTarget::OpenRouter => "openrouter".to_owned(),
        AccountTarget::Unspecified => "<account>".to_owned(),
        AccountTarget::Named(account) => account.as_str().to_owned(),
    }
}

/// OpenRouter is an API key, not a native login.
fn credential_command(account: AccountTarget) -> String {
    match account {
        AccountTarget::OpenRouter => "plakd key set openrouter".to_owned(),
        AccountTarget::Named(account)
            if account
                .as_str()
                .parse::<plakous_hosted::account::AccountRef>()
                .is_ok_and(|account| account.vertical().is_api()) =>
        {
            format!("plakd key set {}", account.as_str())
        }
        other => format!("plakd login {}", account_label(other)),
    }
}

/// Legacy stores never count against this build's store.
const LEGACY_STORES: &str =
    "`plakd state prune-legacy` frees only other builds' stores, not this one";

/// `the session store is <file>; ` for this process's product home, so a
/// capacity remedy names the file it is about.
fn session_store_clause() -> String {
    crate::home::paths::PlakPaths::home_from_env().map_or_else(
        |_| String::new(),
        |paths| {
            format!(
                "the session store is {}; ",
                paths.runtime_database().display()
            )
        },
    )
}

pub(crate) fn storage_admission(text: &str) -> String {
    format!(
        "{text}; {}",
        crate::refusal_text::advice_text(
            &plakous_host_protocol::refusal::RefusalAdvice::CheckStorage,
            Frontend::Run,
        )
    )
}

pub(crate) fn with_remedy(
    text: &str,
    remedy: Option<&NoticeRemedy>,
    context: RemedyContext<'_>,
) -> String {
    let Some(remedy) = remedy else {
        return text.to_owned();
    };
    let help = remedy_text(remedy, context);
    if help.is_empty() {
        text.to_owned()
    } else {
        format!("{text}; {help}")
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "One exhaustive projection keeps all 37 closed semantic remedies and their three frontend alternatives together for review."
)]
pub(crate) fn remedy_text(remedy: &NoticeRemedy, context: RemedyContext<'_>) -> String {
    use Frontend::{Jsonl, Run, Wire};
    use NoticeRemedy as R;
    let face = context.frontend;
    let session = context.session_id.unwrap_or("<session>");
    let open_client = format!("attach an external client to session {session}");
    match remedy {
        R::ResumeAfterLoss {} => match face {
            Wire => "use recover (or resume_queue) to resume everything the lost host interrupted, r on one agent in the client agent controls, or stop all agents to end it".into(),
            Run => format!("{open_client} and use recover to resume what the lost host interrupted"),
            Jsonl => "send resume to resume everything the lost host interrupted, or resume with agent_id for one agent".into(),
        },
        R::RecoverChildren {} => match face {
            Wire => "\
                recover selects unresolved child sessions only; u on an agent in the Agents \
                tab retries a particular obligation; close_agent only ends a node whose \
                reported remedy is close_agent"
                .into(),
            Run => format!(
                "`plakd session recover {session}` selects unresolved child sessions only; \
                 use the client agent controls for per-agent recovery; close_agent only ends a \
                 node whose reported remedy is close_agent"
            ),
            Jsonl => "\
                send recover for unresolved child sessions; invoke_tool with recover_agent or \
                close_agent and its existing typed arguments handles per-child obligations; \
                root recovery does not select an already-settled child's pending tree publication"
                .into(),
        },
        R::Rebind {} => match face {
            Wire => "use rebind, or fork while attached, or start a new session".into(),
            Run => format!("restore the original account/configuration for exact resume, or {open_client} and use rebind to carry history"),
            Jsonl => "restore the original account/configuration and reopen exactly, or use an external client to rebind/carry history; JSONL fork does not rebind an arbitrary profile".into(),
        },
        R::Login { account } => format!(
            "run `{}` in a shell, then {}",
            credential_command(*account),
            match face {
                Wire => "retry attach when detached",
                Run => "retry the run",
                Jsonl => "retry or reopen the agent",
            }
        ),
        R::RetryThenLogin { account } => {
            format!("retry the prompt; if it persists, run `{}`", credential_command(*account))
        }
        R::StopProcesses {} => match face {
            Wire => "stop running processes with job controls before retrying the transition".into(),
            _ => format!("{open_client} and stop running processes with job controls before retrying the transition; JSONL has no process-stop command"),
        },
        R::Attach {} => match face {
            Wire => "attach reopens the session and recovers interrupted work".into(),
            Run => format!("retry `plakd run --session {session} -- <prompt>`; reopening recovers interrupted work"),
            Jsonl => format!("reopen `plakd agent --session {session}`; there is no JSONL attach command"),
        },
        R::ResumeQueueOrStartFresh {} => match face {
            Wire => "review the preceding compaction result and correct its cause; resume_queue resumes the queue; compact is a new paid operation; new session starts a new session".into(),
            Run => format!("inspect the prior result, {open_client} to resume the queue or retry compact as a new paid operation, or start a fresh run"),
            Jsonl => "review the preceding compaction outcome, correct its cause, then send resume_queue; compact is a new paid operation and clear starts fresh".into(),
        },
        R::ResumeSavedSession { session_id } => match face {
            Wire => format!("resume {session_id} opens the saved session"),
            Run => format!("resume the saved target with `plakd run --session {session_id} -- <prompt>`"),
            Jsonl => format!("reopen the saved target with `plakd agent --session {session_id}`"),
        },
        R::RetryCompaction {} => match face {
            Wire => "compact retries; a retry is a new paid operation".into(),
            Run => format!("{open_client} and use compact; a retry is a new paid operation"),
            Jsonl => "send compact; a retry is a new paid operation".into(),
        },
        R::AttachOnlyWhenDetached {} => match face {
            Wire => "already attached; attach is only needed when detached".into(),
            Run => String::new(),
            Jsonl => "already attached; reopening is only needed for a detached host".into(),
        },
        R::ResumeTree {} => String::new(),
        R::StartFresh {} => match face {
            Wire => "use new session, or start a new session on the chosen model".into(),
            Run => "start a new `plakd run` without --session".into(),
            Jsonl => "send clear, or launch a fresh plakd agent without --session".into(),
        },
        R::RetryOrChooseProfile {} => match face {
            Wire => "try again, or choose another profile with profile actions".into(),
            Run => format!(
                "retry, select an admitted account/model/effort for a new run, or {open_client} \
                 for profile actions"
            ),
            Jsonl => "retry the prompt, or select another profile through an external client".into(),
        },
        R::RecoverSession {} => match face {
            Wire => "the next prompt or recover retries recovery".into(),
            Run => format!("submit the next run prompt, or run `plakd session recover {session}`"),
            Jsonl => "send recover, or submit the next prompt".into(),
        },
        R::InterruptWait {} | R::CancelContinuation {} => match face {
            Wire => "Interrupt cancels the wait or automatic continuation".into(),
            Run => "Ctrl-C to stop".into(),
            Jsonl => "send interrupt to cancel the wait or automatic continuation".into(),
        },
        R::ResumeExactSession {} => match face { Wire => "`resume <uuid>` accepts any exact session ID".into(), _ => String::new() },
        R::ChooseProfile {} | R::ChooseProfileEffort {} | R::ChooseProfileOrAttach {} => match face {
            Wire => match remedy {
                R::ChooseProfileEffort {} => {
                    "choose a profile whose root names an explicit effort with profile actions".into()
                }
                R::ChooseProfileOrAttach {} => "\
                    choose a profile with profile actions for automatic attachment, or retry attach \
                    after repair"
                    .into(),
                _ => "choose a profile with profile actions; the agent attaches automatically".into(),
            },
            Run => format!("select an admitted account/model/effort for a new run, or {open_client} for profile actions"),
            Jsonl => "select account/model/explicit effort at a fresh agent launch, or use an external client profile actions for profile selection; there is no JSONL profile picker or attach command".into(),
        },
        R::RepairJavaScriptHost {} | R::RepairPythonHost {} => {
            let (env, binary) = if matches!(remedy, R::RepairJavaScriptHost {}) { ("PLAK_JS_HOST", "plakous-js-host") } else { ("PLAK_PY_HOST", "plakous-py-host") };
            let retry = match face { Wire => "attach".into(), Run => "retry the run".into(), Jsonl => format!("reopen `plakd agent --session {session}`") };
            format!("set {env} or place {binary} beside plakd, then {retry}")
        },
        R::CarryCurrentLogin {} => match face {
            Wire => "use rebind to carry history under the current login; same-account login cannot restore the old selection; fork is available only while attached".into(),
            _ => format!(
                "{open_client} and use rebind to carry full history under the current login; \
                 same-account login cannot restore the old selection; exact resume requires the \
                 original selection"
            ),
        },
        R::RestoreAccountOrRebind { account } => {
            let login = credential_command(*account);
            match face {
                Wire => format!(
                    "restore the original account/configuration with `{login}` for exact reopen, \
                     or rebind to carry full history under the \
                     current login; profile actions cannot repair a frozen binding"
                ),
                _ => format!(
                    "restore the original account/configuration with `{login}` for exact reopen, \
                     or {open_client} and rebind to carry full history under the current login; \
                     selecting a profile cannot repair a frozen binding"
                ),
            }
        },
        R::RestoreBindingOrCarry { account } => {
            let login = credential_command(*account);
            match face {
                Wire => format!(
                    "carry the full history into a new session with rebind; restoring the \
                     original account/configuration with `{login}` reopens it exactly instead; \
                     profile actions cannot repair a frozen binding"
                ),
                _ => format!(
                    "{open_client} and use rebind to carry the full history into a new session; \
                     restoring the original account/configuration with `{login}` reopens it \
                     exactly instead; selecting a profile cannot repair a frozen binding"
                ),
            }
        },
        R::LoginThenAttach { account } => {
            let retry = match face { Wire => "retry attach".into(), Run => "retry the run".into(), Jsonl => format!("reopen `plakd agent --session {session}`") };
            format!(
                "run `{}` in a shell, then {retry}; a same-account Claude login preserves the session where the route attests it",
                credential_command(*account)
            )
        },
        R::ContinueOnAccount { account, .. } => match face {
            Wire => format!("choose Continue on {account} to submit it"),
            Run => format!("run the prompt again with `--account` naming {account}"),
            Jsonl => format!("submit the prompt again on {account}"),
        },
        R::RepairThenAttach {} => match face {
            Wire => "\
                address the attach failure, then retry attach; profile actions only selects a different \
                profile"
                .into(),
            _ => format!(
                "address the attach failure, then reopen the session; {open_client} to select a \
                 different profile as a separate action"
            ),
        },
        R::RepairCredentialOrChooseProfile { account, condition } => {
            let login = credential_command(*account);
            let repair = match condition {
                CredentialCondition::NewerBuild => format!("use the writing build, or replace its login with `{login}`; the writing build must then log in again"),
                CredentialCondition::OlderProfile => format!("replace the older-build/profile login with `{login}`"),
                CredentialCondition::Rejected | CredentialCondition::Unavailable => format!("run `{login}`"),
            };
            format!("{repair}; {}", if face == Wire { "or choose another profile with profile actions" } else { "or select another profile through an external client" })
        },
        R::CompactWholeGroups {} | R::CompactNative {} => match face {
            Wire => "compact may attempt compaction of this exact checkpoint; it does not guarantee the retained projection will fit".into(),
            Run => format!("{open_client} and use compact on this exact checkpoint; no fit is guaranteed; otherwise use a new run or an admitted larger model"),
            Jsonl => "send compact for this exact checkpoint; no fit is guaranteed".into(),
        },
        R::CompactSegmentHistory {} => match face {
            Wire => "compact reduces this session; fork with compaction reduces a fork and leaves this session unchanged; new session starts a new session".into(),
            Run => format!("{open_client} for compact, fork with compaction or new session, with their distinct effects, or start a fresh run"),
            Jsonl => "compact reduces this session; fork with compact: true reduces a fork; clear starts fresh".into(),
        },
        R::LargerModelOrStartFresh {} => match face {
            Wire => "\
                choose a larger model, fork, or new session; the failed turn is not automatically \
                replayed"
                .into(),
            Run => format!(
                "start a new run with a larger model, or {open_client} for an explicit fork; no \
                 automatic replay"
            ),
            Jsonl => "\
                select a larger model in a new session, fork where admissible, or send clear; no \
                automatic replay"
                .into(),
        },
        R::RetryFork {} => match face { Wire => "run fork again".into(), Run => format!("{open_client} and use fork"), Jsonl => "send fork again".into() },
        R::ReclaimThenRecover {} => format!(
            "{}run `plakd session gc --all`, then `plakd session delete <id>` on unneeded \
             sessions, then {}; {LEGACY_STORES}",
            session_store_clause(),
            match face {
                Wire => "recover".into(),
                Run => format!("`plakd session recover {session}`"),
                Jsonl => "send recover".into(),
            }
        ),
        R::ReclaimRetainedHistory {} => format!(
            "{}run `plakd session gc --all`, then `plakd session delete <id>` on unneeded \
             sessions; {} do not reclaim retained history; {LEGACY_STORES}",
            session_store_clause(),
            match face {
                Wire => "new session, compaction and recover",
                _ => "clear, compact and recover",
            }
        ),
        R::FreeDiskSpace {} => "run `plakd session gc --all`, delete unneeded sessions with `plakd session delete <id>`, or free disk space; the logical reserve does not reserve disk blocks".into(),
    }
}
