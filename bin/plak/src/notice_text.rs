//! Frontend-specific operator help selected only from semantic remedy codes.

use plakous_host_protocol::notice_remedy::{AccountTarget, CredentialCondition, NoticeRemedy};

#[cfg(test)]
pub(crate) mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Frontend {
    Tui,
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
            Frontend::Tui => "press Tab to focus the queue, then r to resume delivery or Enter to send one item now".to_owned(),
            Frontend::Run => format!("open `plak --resume {}`; Tab then r resumes delivery, or Enter sends one item now", context.session_id.unwrap_or("<session>")),
            Frontend::Jsonl => "send resume_queue to drain; selecting and sending one item remains a TUI action".to_owned(),
        });
    }
    text
}

pub(crate) const fn account_label(account: AccountTarget) -> &'static str {
    match account {
        AccountTarget::Codex => "codex",
        AccountTarget::Grok => "grok",
        AccountTarget::Claude => "claude",
        AccountTarget::OpenRouter => "openrouter",
        AccountTarget::Unspecified => "<account>",
    }
}

/// OpenRouter is an API key, not a native login.
fn credential_command(account: AccountTarget) -> String {
    match account {
        AccountTarget::OpenRouter => "plak key set openrouter".to_owned(),
        other => format!("plak login {}", account_label(other)),
    }
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
    reason = "One exhaustive projection keeps all 36 closed semantic remedies and their three frontend alternatives together for review."
)]
pub(crate) fn remedy_text(remedy: &NoticeRemedy, context: RemedyContext<'_>) -> String {
    use Frontend::{Jsonl, Run, Tui};
    use NoticeRemedy as R;
    let face = context.frontend;
    let session = context.session_id.unwrap_or("<session>");
    let open_tui = format!("open `plak --resume {session}`");
    match remedy {
        R::RecoverChildren {} => match face {
            Tui => "\
                /recover selects unresolved child sessions only; recover_agent retries a \
                particular obligation; close_agent only ends a node whose reported remedy is \
                close_agent"
                .into(),
            Run => format!(
                "`plak session recover {session}` selects unresolved child sessions only; \
                 use the TUI for per-agent recovery; recover_agent retries an obligation \
                 and close_agent only ends a node whose reported remedy is close_agent"
            ),
            Jsonl => "\
                send recover for unresolved child sessions; invoke_tool with recover_agent or \
                close_agent and its existing typed arguments handles per-child obligations; \
                root recovery does not select an already-settled child's pending tree publication"
                .into(),
        },
        R::Rebind {} => match face {
            Tui => "use /rebind, or Fork now in the Agents tab while attached, or start a new session".into(),
            Run => format!("restore the original account/configuration for exact resume, or {open_tui} and use /rebind to carry history"),
            Jsonl => "restore the original account/configuration and reopen exactly, or use the TUI to rebind/carry history; JSONL fork does not rebind an arbitrary profile".into(),
        },
        R::Login { account } => format!(
            "run `{}` in a shell, then {}",
            credential_command(*account),
            match face {
                Tui => "retry /attach when detached",
                Run => "retry the run",
                Jsonl => "retry or reopen the agent",
            }
        ),
        R::RetryThenLogin { account } => {
            format!("retry the prompt; if it persists, run `{}`", credential_command(*account))
        }
        R::StopProcesses {} => match face {
            Tui => "stop running processes with /ps before retrying the transition".into(),
            _ => format!("{open_tui} and stop running processes with /ps before retrying the transition; JSONL has no process-stop command"),
        },
        R::Attach {} => match face {
            Tui => "/attach reopens the session and recovers interrupted work".into(),
            Run => format!("retry `plak run --session {session} -- <prompt>`; reopening recovers interrupted work"),
            Jsonl => format!("reopen `plak agent --session {session}`; there is no JSONL attach command"),
        },
        R::ResumeQueueOrStartFresh {} => match face {
            Tui => "review the preceding compaction result and correct its cause; Tab then r resumes the queue; /compact is a new paid operation; /new starts a new session".into(),
            Run => format!("inspect the prior result, {open_tui} to resume the queue or retry /compact as a new paid operation, or start a fresh run"),
            Jsonl => "review the preceding compaction outcome, correct its cause, then send resume_queue; compact is a new paid operation and clear starts fresh".into(),
        },
        R::ResumeSavedSession { session_id } => match face {
            Tui => format!("/resume {session_id} opens the saved session"),
            Run => format!("resume the saved target with `plak run --session {session_id} -- <prompt>`"),
            Jsonl => format!("reopen the saved target with `plak agent --session {session_id}`"),
        },
        R::RetryCompaction {} => match face {
            Tui => "/compact retries; a retry is a new paid operation".into(),
            Run => format!("{open_tui} and use /compact; a retry is a new paid operation"),
            Jsonl => "send compact; a retry is a new paid operation".into(),
        },
        R::AttachOnlyWhenDetached {} => match face {
            Tui => "already attached; /attach is only needed when detached".into(),
            Run => String::new(),
            Jsonl => "already attached; reopening is only needed for a detached host".into(),
        },
        R::ResumeTree {} => String::new(),
        R::StartFresh {} => match face {
            Tui => "use /new, or start a new session on the chosen model".into(),
            Run => "start a new `plak run` without --session".into(),
            Jsonl => "send clear, or launch a fresh plak agent without --session".into(),
        },
        R::RetryOrChooseProfile {} => match face {
            Tui => "try again, or choose another profile with /agents".into(),
            Run => format!(
                "retry, select an admitted account/model/effort for a new run, or {open_tui} \
                 for profile actions"
            ),
            Jsonl => "retry the prompt, or select another profile through the TUI".into(),
        },
        R::RecoverSession {} => match face {
            Tui => "the next prompt or /recover retries recovery".into(),
            Run => format!("submit the next run prompt, or run `plak session recover {session}`"),
            Jsonl => "send recover, or submit the next prompt".into(),
        },
        R::InterruptWait {} | R::CancelContinuation {} => match face {
            Tui => "Esc cancels the wait or automatic continuation".into(),
            Run => "Ctrl-C to stop".into(),
            Jsonl => "send interrupt to cancel the wait or automatic continuation".into(),
        },
        R::ResumeExactSession {} => match face { Tui => "`/resume <uuid>` accepts any exact session ID".into(), _ => String::new() },
        R::ChooseProfile {} | R::ChooseProfileEffort {} | R::ChooseProfileOrAttach {} => match face {
            Tui => match remedy {
                R::ChooseProfileEffort {} => {
                    "choose a profile whose root names an explicit effort with /agents".into()
                }
                R::ChooseProfileOrAttach {} => "\
                    choose a profile with /agents for automatic attachment, or retry /attach \
                    after repair"
                    .into(),
                _ => "choose a profile with /agents; the agent attaches automatically".into(),
            },
            Run => format!("select an admitted account/model/effort for a new run, or {open_tui} for profile actions"),
            Jsonl => "select account/model/explicit effort at a fresh agent launch, or use the TUI /agents for profile selection; there is no JSONL profile picker or attach command".into(),
        },
        R::RepairJavaScriptHost {} | R::RepairPythonHost {} => {
            let (env, binary) = if matches!(remedy, R::RepairJavaScriptHost {}) { ("PLAK_JS_HOST", "plakous-js-host") } else { ("PLAK_PY_HOST", "plakous-py-host") };
            let retry = match face { Tui => "/attach".into(), Run => "retry the run".into(), Jsonl => format!("reopen `plak agent --session {session}`") };
            format!("set {env} or place {binary} beside plak, then {retry}")
        },
        R::CarryCurrentLogin {} => match face {
            Tui => "use /reload or /rebind to carry history under the current login; same-account login cannot restore the old selection; Fork now is available only while attached".into(),
            _ => format!(
                "{open_tui} and use /rebind to carry full history under the current login; \
                 same-account login cannot restore the old selection; exact resume requires the \
                 original selection"
            ),
        },
        R::RestoreAccountOrRebind { account } => {
            let login = credential_command(*account);
            match face {
                Tui => format!(
                    "restore the original account/configuration with `{login}` for exact reopen, \
                     or /rebind (also /reload after this refusal) to carry full history under the \
                     current login; /agents cannot repair a frozen binding"
                ),
                _ => format!(
                    "restore the original account/configuration with `{login}` for exact reopen, \
                     or {open_tui} and /rebind to carry full history under the current login; \
                     selecting a profile cannot repair a frozen binding"
                ),
            }
        },
        R::RestoreBindingOrCarry { account } => {
            let login = credential_command(*account);
            match face {
                Tui => format!(
                    "carry the full history into a new session with /rebind; restoring the \
                     original account/configuration with `{login}` reopens it exactly instead; \
                     /agents cannot repair a frozen binding"
                ),
                _ => format!(
                    "{open_tui} and use /rebind to carry the full history into a new session; \
                     restoring the original account/configuration with `{login}` reopens it \
                     exactly instead; selecting a profile cannot repair a frozen binding"
                ),
            }
        },
        R::LoginThenAttach { account } => {
            let retry = match face { Tui => "retry /attach".into(), Run => "retry the run".into(), Jsonl => format!("reopen `plak agent --session {session}`") };
            format!(
                "run `{}` in a shell, then {retry}; a same-account Claude login preserves the session where the route attests it",
                credential_command(*account)
            )
        },
        R::RepairThenAttach {} => match face {
            Tui => "\
                address the attach failure, then retry /attach; /agents only selects a different \
                profile"
                .into(),
            _ => format!(
                "address the attach failure, then reopen the session; {open_tui} to select a \
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
            format!("{repair}; {}", if face == Tui { "or choose another profile with /agents" } else { "or select another profile through the TUI" })
        },
        R::CompactWholeGroups {} | R::CompactNative {} => match face {
            Tui => "/compact may attempt compaction of this exact checkpoint; it does not guarantee the retained projection will fit".into(),
            Run => format!("{open_tui} and use /compact on this exact checkpoint; no fit is guaranteed; otherwise use a new run or an admitted larger model"),
            Jsonl => "send compact for this exact checkpoint; no fit is guaranteed".into(),
        },
        R::CompactSegmentHistory {} => match face {
            Tui => "/compact reduces this session; /fork-then-compact reduces a fork and leaves this session unchanged; /new starts a new session".into(),
            Run => format!("{open_tui} for /compact, /fork-then-compact or /new, with their distinct effects, or start a fresh run"),
            Jsonl => "compact reduces this session; fork with compact: true reduces a fork; clear starts fresh".into(),
        },
        R::LargerModelOrStartFresh {} => match face {
            Tui => "\
                choose a larger model, fork, or /new; the failed turn is not automatically \
                replayed"
                .into(),
            Run => format!(
                "start a new run with a larger model, or {open_tui} for an explicit fork; no \
                 automatic replay"
            ),
            Jsonl => "\
                select a larger model in a new session, fork where admissible, or send clear; no \
                automatic replay"
                .into(),
        },
        R::RetryFork {} => match face { Tui => "run /fork again".into(), Run => format!("{open_tui} and use /fork"), Jsonl => "send fork again".into() },
        R::ReclaimThenRecover {} => format!(
            "run `plak session gc --all`, then `plak session delete <id>` on unneeded sessions, \
             then {}",
            match face {
                Tui => "/recover".into(),
                Run => format!("`plak session recover {session}`"),
                Jsonl => "send recover".into(),
            }
        ),
        R::ReclaimRetainedHistory {} => format!(
            "run `plak session gc --all`, then `plak session delete <id>` on unneeded sessions; \
             {} do not reclaim retained history",
            match face {
                Tui => "/new, compaction and /recover",
                _ => "clear, compact and recover",
            }
        ),
        R::FreeDiskSpace {} => "run `plak session gc --all`, delete unneeded sessions with `plak session delete <id>`, or free disk space; the logical reserve does not reserve disk blocks".into(),
    }
}
