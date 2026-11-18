//! Typed driver extensions. Only immutable facts and bounded observations cross this port.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::Sender;
use crate::binding::RootTemplateFact;
use crate::sessions::{NoticeScope, TranscriptRow};
use plakous_host_protocol::views::{ProviderDef, ProviderSection};
use plakous_host_protocol::{Core, ProtocolExtension};

impl<P: ProtocolExtension> Sender<P> {
    /// Whether this request belongs to a local face. Transport-stamped
    /// local attachment provenance precedes any client correlation. Missing
    /// or unrecognized provenance fails closed. Answering clones retain their source.
    #[must_use]
    pub fn is_local_request(&self) -> bool {
        let window = self
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.answers
            .as_ref()
            .or(window.as_ref())
            .and_then(|started| started.0.as_deref())
            .is_some_and(|origin| {
                origin
                    .strip_prefix(crate::host_service::ANSWER)
                    .unwrap_or(origin)
                    .starts_with(crate::host_service::LOCAL_ATTACHMENT)
            })
    }
}

/// Why an extension intent could not be answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceRefusal {
    /// No product handler was installed.
    NoHandler,
}
impl SurfaceRefusal {
    /// Safe operator-facing explanation.
    #[must_use]
    pub const fn note(self) -> &'static str {
        match self {
            Self::NoHandler => {
                "product surfaces are unavailable in this session (no surface handler)"
            }
        }
    }
}

/// The host phase an extension may observe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfacePhase {
    /// Attached and idle.
    Idle,
    /// One admitted turn is active.
    Busy,
    /// No canonical attachment exists.
    Detached,
}

/// Immutable facts available while handling one extension intent.
pub struct SurfaceFacts<'a> {
    /// Current host phase.
    pub phase: SurfacePhase,
    /// Whether the attached host was configured for one-shot execution.
    pub attached_one_shot: bool,
    /// Admitted provider/model catalog.
    pub catalog: &'a [ProviderSection],
    /// Number of retained queued prompts.
    pub queue_len: usize,
    /// Current session's presentation facts, never reconstructed from defaults.
    pub session: Option<SurfaceSession>,
    /// The host's queued and running subscription usage reads.
    pub usage_reads: UsageReads,
    /// Every known account's readiness, read off the host loop before the
    /// dispatch when [`SurfaceHandler::accounts_read`] asked for it.
    pub accounts: Option<&'a AccountReadiness>,
}

/// The host's queued and running subscription usage reads, as a surface waits
/// on them.
///
/// Usage reads retain provider Ready leases across their network exchanges,
/// not the shared compatibility guard. A surface that reads subscription
/// posture for a Usage intent still does so off the action loop and waits
/// here first, so it reports the settled credential lifecycle.
#[derive(Clone, Debug, Default)]
pub struct UsageReads {
    pending: Arc<(Mutex<usize>, Condvar)>,
}

impl UsageReads {
    /// Blocks until no usage read is queued or running, or `limit` passes;
    /// returns whether the reads settled.
    #[must_use]
    pub fn wait_settled(&self, limit: Duration) -> bool {
        let (pending, settled) = &*self.pending;
        let Ok(guard) = pending.lock() else {
            return false;
        };
        settled
            .wait_timeout_while(guard, limit, |pending| *pending > 0)
            .is_ok_and(|(pending, _)| *pending == 0)
    }

    /// Marks one read queued or running until the returned guard drops.
    #[must_use]
    pub fn begin(&self) -> UsageRead {
        if let Ok(mut pending) = self.pending.0.lock() {
            *pending = pending.saturating_add(1);
        }
        UsageRead(self.clone())
    }
}

/// One queued or running usage read; dropping it settles the read.
#[derive(Debug)]
pub struct UsageRead(UsageReads);

impl Drop for UsageRead {
    fn drop(&mut self) {
        let (pending, settled) = &*self.0.pending;
        if let Ok(mut pending) = pending.lock() {
            *pending = pending.saturating_sub(1);
        }
        settled.notify_all();
    }
}

/// A product's credential readiness re-read: provider rows and model sections,
/// or a redaction-safe failure. The host runs it on its own thread.
pub type ReadinessRead =
    Box<dyn FnOnce() -> Result<(Vec<ProviderDef>, Vec<ProviderSection>), String> + Send>;

/// One account a session account choice may select (spec 02 §4.4.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadyAccount {
    /// Full account reference.
    pub account: String,
    /// The credential can serve a new dispatch now.
    pub ready: bool,
    /// Operator-facing label, when the catalog has one.
    pub label: Option<String>,
}

/// Every known account with its readiness, or a redaction-safe reason the
/// read failed. A failed read is never an empty catalog.
pub type AccountReadiness = Result<Vec<ReadyAccount>, String>;

/// A product's account readiness read. It may wait on a credential gateway
/// or subscription locks, so the host runs it on its own thread and awaits
/// it; the host loop never blocks on it.
pub type AccountsRead = Box<dyn FnOnce() -> AccountReadiness + Send>;

/// A product's read of the other Ready accounts of one account's kind, run
/// like [`AccountsRead`].
pub type AlternativesRead = Box<dyn FnOnce() -> Vec<String> + Send>;

/// Frozen session facts for product-owned identity and workspace presentation.
pub struct SurfaceSession {
    /// Saved execution placement; absence denotes the host-local runner.
    pub runner: Option<crate::sessions::RunnerDisplay>,
    /// The product session this host serves.
    pub id: String,
    /// Exact root account.
    pub account: Option<String>,
    /// Exact root model.
    pub model: Option<String>,
    /// Root effort; absence means default effort.
    pub effort: Option<String>,
    /// Root fast-lane selection.
    pub fast: bool,
    /// Launch workspace, not the process's mutable working directory.
    pub workspace: std::path::PathBuf,
    /// This session's frozen tree epoch, when it has one.
    pub roster: Option<crate::orchestration::OrchestrationSnapshot>,
    /// Whether a sub-agent of this session's attached tree is running or
    /// parked: work a reload carrying into a new session cannot hand off.
    pub live_children: bool,
    /// The MCP tools this session froze at creation; `None` unless the
    /// handler asked for them ([`SurfaceHandler::needs_session_tools`]).
    pub mcp: Option<SurfaceMcp>,
    /// The stored account choice of every non-root kind, kind to account.
    pub account_choices: std::collections::BTreeMap<String, String>,
}

/// A session's frozen MCP tools, for an operator view.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SurfaceMcp {
    /// Every server the session's frozen template lists, in id order.
    pub servers: Vec<String>,
    /// `(server id, MCP tool name)` per bound tool, in bind order.
    pub tools: Vec<(String, String)>,
    /// The omission notices of the creation bind still in the transcript.
    pub omitted: Vec<String>,
    /// The canonical `[mcp_servers.<id>]` table each listed server was bound
    /// with, keyed by id.
    pub definitions: std::collections::BTreeMap<String, String>,
}

impl SurfaceMcp {
    fn of(session: &crate::sessions::SessionRecord) -> Self {
        let Some(selection) = session
            .frozen_tools
            .as_ref()
            .map(crate::frozen_tools::FrozenRootTools::selection)
        else {
            return Self::default();
        };
        let omitted = session
            .transcript
            .rows()
            .into_iter()
            .filter_map(|(row, _)| match row {
                TranscriptRow::Notice(notice)
                    if notice.text.starts_with("mcp tool ")
                        && notice.text.ends_with("; omitted") =>
                {
                    Some(notice.text.clone())
                }
                _ => None,
            })
            .collect();
        Self {
            servers: selection
                .selection
                .mcp_servers()
                .map(|server| server.as_str().to_owned())
                .collect(),
            tools: selection
                .mcp_tools
                .iter()
                .flatten()
                .map(|record| {
                    (
                        record.server().as_str().to_owned(),
                        record.mcp_name().to_owned(),
                    )
                })
                .collect(),
            omitted,
            definitions: selection
                .tool_sections
                .iter()
                .filter_map(|(key, table)| {
                    key.strip_prefix("mcp_servers.")
                        .map(|id| (id.to_owned(), table.clone()))
                })
                .collect(),
        }
    }
}

pub(super) fn session_facts(
    session: &crate::sessions::SessionRecord,
    rows: Option<&dyn super::topology_state::TopologyRows>,
    with_tools: bool,
) -> SurfaceSession {
    let tree_id = rows.map(|rows| super::topology_state::durable_tree_id(rows, session));
    let hub = tree_id.as_deref().and_then(crate::topology_runtime::lookup);
    let roster = match (&hub, rows, &tree_id) {
        (Some(runtime), _, _) => runtime.read(|tree| tree.current_epoch().clone()).ok(),
        (None, Some(rows), Some(tree_id)) => rows
            .tree(tree_id)
            .ok()
            .flatten()
            .map(|tree| tree.current_epoch().clone()),
        _ => None,
    };
    let live_children = hub.is_some_and(|runtime| {
        runtime
            .read(|tree| {
                tree.nodes().values().any(|node| {
                    node.parent_agent_id.is_some()
                        && node.status == crate::agent_tree_protocol::NodeStatus::Running
                })
            })
            .unwrap_or(false)
    });
    SurfaceSession {
        runner: session.runner.clone(),
        id: session.id.clone(),
        account: session.account.clone(),
        model: session.model.clone(),
        effort: session.effort.clone(),
        fast: session.target_selection == crate::binding::TargetSelection::NativePriority,
        workspace: session.workspace_root.clone(),
        roster,
        live_children,
        mcp: with_tools.then(|| SurfaceMcp::of(session)),
        account_choices: session.account_choices.clone(),
    }
}

/// A durable product configuration replacement the host may project.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SurfaceSaved {
    /// Root template selected for subsequent construction.
    pub root_template: Option<RootTemplateFact>,
}

/// How a profile the product asked the host to use now ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileForkOutcome {
    /// The attached session branched onto the profile. The previous
    /// session stays resumable on its own tree and cards.
    Forked,
    /// The root node continued on its own line as the profile: its
    /// children and processes kept running (Switch now).
    Switched,
    /// A detached session with no conversation took the profile's root.
    Bound,
    /// This session already runs that root with those cards.
    AlreadyActive,
    /// Nothing changed; the reason is operator-facing.
    Refused(plakous_host_protocol::status::HostFailure),
}

/// A profile the product wants the host to use now.
///
/// The product has validated the profile but saved nothing. The host
/// branches the attached session onto `account`/`model` with a tree of its
/// own that freezes `roster` (or binds a detached, empty session), then calls
/// `finish` exactly once with the outcome. The product persists its pointers
/// only from `finish`, so a refused fork changes no default.
pub struct SurfaceProfileFork {
    /// Account the root binds.
    pub account: String,
    /// Model the root binds.
    pub model: String,
    /// Effort, when the profile names one.
    pub effort: Option<String>,
    /// Processing extra, when the profile's root is on the fast lane.
    pub extra: Option<String>,
    /// The root template's composition, when it names one.
    pub composition: Option<crate::binding::CompositionSelection>,
    /// The root template's `OpenRouter` wire, when it names one. Only an
    /// `openrouter` root may carry one.
    pub protocol: Option<crate::binding::ProtocolSelection>,
    /// The profile's working set, frozen as the new session's tree epoch.
    pub roster: crate::orchestration::RosterInput,
    /// `Some(profile name)` continues the root node on its own line as that
    /// profile (Switch now) instead of forking a new session.
    pub switch: Option<String>,
    /// The profile's filled non-root account choices, which the successor
    /// takes; `None` keeps the session's.
    pub account_choices: Option<std::collections::BTreeMap<String, String>>,
    /// Receives the outcome.
    pub finish: Box<dyn FnOnce(ProfileForkOutcome) + Send>,
}

impl SurfaceProfileFork {
    /// Ends the request without changing anything.
    pub fn refuse(self, reason: impl Into<plakous_host_protocol::status::HostFailure>) {
        (self.finish)(ProfileForkOutcome::Refused(reason.into()));
    }
}

impl std::fmt::Debug for SurfaceProfileFork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SurfaceProfileFork")
            .field("account", &self.account)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("extra", &self.extra)
            .finish_non_exhaustive()
    }
}

/// An account choice the product validated (spec 02 §4.4.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceAccountChoice {
    /// Store `account` as the session's choice for the non-root `kind`.
    /// Nothing rebinds; new spawns of that kind use it.
    Store {
        /// Provider kind.
        kind: String,
        /// Full account reference of that kind.
        account: String,
    },
    /// Continue the root on `account` through the rebind workflow. With
    /// `continue_failed`, the line head's failed root turn is submitted again
    /// once the rebind publishes.
    Root {
        /// Full account reference of the root kind.
        account: String,
        /// Submit the failed root turn again.
        continue_failed: bool,
    },
}

/// Why the host did not apply an account choice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccountChoiceRefusal {
    /// The root is running a turn; a root-kind choice waits for idle.
    RootBusy,
    /// A continue needs the line head's last root turn to have failed.
    NoFailedTurn,
    /// This continue was already submitted for that turn and account.
    AlreadyContinued,
    /// Nothing changed; the reason is operator-facing.
    Failed(plakous_host_protocol::status::HostFailure),
}

/// How an account choice ended. `Chosen` carries the line head's root
/// account and stored choices after it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccountChoiceOutcome {
    /// The choice holds.
    Chosen {
        /// The line head's root account.
        root: Option<String>,
        /// The line head's stored non-root choices.
        choices: std::collections::BTreeMap<String, String>,
    },
    /// Nothing changed.
    Refused(AccountChoiceRefusal),
}

/// An account choice for the host to apply, and where its outcome goes.
pub struct SurfaceAccountEffect {
    /// What to apply.
    pub choice: SurfaceAccountChoice,
    /// Receives the outcome exactly once.
    pub finish: Box<dyn FnOnce(AccountChoiceOutcome) + Send>,
}

impl SurfaceAccountEffect {
    /// Ends the request without changing anything.
    pub fn refuse(self, refusal: AccountChoiceRefusal) {
        (self.finish)(AccountChoiceOutcome::Refused(refusal));
    }
}

impl std::fmt::Debug for SurfaceAccountEffect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SurfaceAccountEffect")
            .field("choice", &self.choice)
            .finish_non_exhaustive()
    }
}

/// The host-owned consequence of an extension intent.
#[derive(Debug, Default)]
pub struct SurfaceEffect {
    /// A completed durable configuration save, if any.
    pub saved: Option<SurfaceSaved>,
    /// Use this profile now.
    pub profile_fork: Option<SurfaceProfileFork>,
    /// Apply this account choice.
    pub account: Option<SurfaceAccountEffect>,
}

/// Driver policy for its own typed extension. No canonical authority is exposed.
pub trait SurfaceHandler<P: ProtocolExtension>: Send {
    /// Current home-only idle continuation policy; None retains the attach fact.
    fn child_wake_enabled(&self) -> Option<bool> {
        None
    }

    /// Whether the host should publish its usage/context facts before dispatch.
    fn needs_usage(&self, _intent: &P::Intent) -> bool {
        false
    }
    /// Bypass held allowance freshness after an explicit credential mutation.
    fn fresh_usage(&self, _intent: &P::Intent) -> bool {
        false
    }

    /// Whether the Usage tab is open, so a busy turn re-reads allowances.
    fn usage_watch_armed(&self) -> bool {
        false
    }

    /// The host is about to leave `session_id` for another session it serves
    /// in place (`/new`, `/resume`, a fork or a rebind). State the product
    /// keeps for that session settles here, before the switch completes:
    /// nothing of it carries over to the next session.
    fn leaving_session(&self, _session_id: &str) {}

    /// The read of the other accounts of `account`'s kind that are Ready
    /// now, for a failed turn's usage-limit notice; `None` reports none.
    fn alternatives_read(&self, _account: &str) -> Option<AlternativesRead> {
        None
    }

    /// The account readiness `intent`'s dispatch reads
    /// ([`SurfaceFacts::accounts`]); `None` when it reads none.
    fn accounts_read(&self, _intent: &P::Intent) -> Option<AccountsRead> {
        None
    }

    /// The host serves `session` on this attachment's line head: after an
    /// open, and after a workflow moved the line head. The product publishes
    /// the session state it owns (its account choices).
    fn serving(&self, _session: &SurfaceSession, _events: &Sender<P>) {}

    /// Whether the dispatch reads the session's frozen MCP tools
    /// ([`SurfaceSession::mcp`]); they are gathered only when it does.
    fn needs_session_tools(&self, _intent: &P::Intent) -> bool {
        false
    }
    /// Answer the driver's intent using immutable facts and bounded observations.
    fn dispatch(
        &self,
        intent: P::Intent,
        facts: SurfaceFacts<'_>,
        events: &Sender<P>,
    ) -> SurfaceEffect;
    /// Publish template readiness for the closed protocol's refresh intent.
    fn publish_template_readiness(&self, catalog: &[ProviderSection], events: &Sender<P>);
    /// The re-read of credential readiness as product data. It may wait on
    /// subscription locks, so the host runs it off the action loop.
    fn readiness_read(&self) -> ReadinessRead;
}

/// Optional driver-owned extension handler.
pub type Surface<P = Core> = Option<Box<dyn SurfaceHandler<P>>>;

/// Tells the surface the host leaves `from` for `to`, when they differ.
pub(super) fn leaving_session<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    from: &str,
    to: &str,
) {
    if from != to
        && let Some(handler) = surface
    {
        handler.leaving_session(from);
    }
}

/// The product's read of the Ready alternatives to `account`, if any.
pub(super) fn alternatives_read<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    account: &str,
) -> Option<AlternativesRead> {
    surface.and_then(|handler| handler.alternatives_read(account))
}

/// Runs the product's alternatives read off the host loop.
pub(super) async fn ready_alternatives(read: Option<AlternativesRead>) -> Vec<String> {
    match read {
        Some(read) => {
            crate::off_loop::await_read("plak-account-alternatives", read, Vec::new).await
        }
        None => Vec::new(),
    }
}

/// The product's account readiness read for `intent`, if its dispatch
/// reads one.
pub(super) fn accounts_read<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    intent: &P::Intent,
) -> Option<AccountsRead> {
    surface.and_then(|handler| handler.accounts_read(intent))
}

/// Runs the product's account readiness read off the host loop.
pub(super) async fn read_accounts(read: Option<AccountsRead>) -> Option<AccountReadiness> {
    let read = read?;
    Some(
        crate::off_loop::await_read("plak-account-readiness", read, || {
            Err("the account readiness read could not run".to_owned())
        })
        .await,
    )
}

/// Tells the surface the host serves `session` now.
pub(super) fn serving<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    session: &crate::sessions::SessionRecord,
    events: &Sender<P>,
) {
    if let Some(handler) = surface {
        handler.serving(&session_facts(session, None, false), events);
    }
}

pub(super) fn needs_session_tools<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    intent: &P::Intent,
) -> bool {
    surface.is_some_and(|handler| handler.needs_session_tools(intent))
}

pub(super) fn fresh_usage<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    intent: &P::Intent,
) -> bool {
    surface.is_some_and(|handler| handler.fresh_usage(intent))
}

pub(super) fn needs_usage<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    intent: &P::Intent,
) -> bool {
    surface.is_some_and(|handler| handler.needs_usage(intent))
}

pub(super) fn usage_watch_armed<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
) -> bool {
    surface.is_some_and(SurfaceHandler::usage_watch_armed)
}

pub(super) fn publish_template_readiness<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    catalog: &[ProviderSection],
    events: &Sender<P>,
) {
    if let Some(handler) = surface {
        handler.publish_template_readiness(catalog, events);
    }
}

pub(super) fn dispatch<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    intent: P::Intent,
    facts: SurfaceFacts<'_>,
    events: &Sender<P>,
) -> SurfaceEffect {
    let Some(handler) = surface else {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Host,
            SurfaceRefusal::NoHandler.note().to_owned(),
        ));
        return SurfaceEffect::default();
    };
    handler.dispatch(intent, facts, events)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts the sessions it was told the host left.
    #[derive(Default)]
    struct Leaving(Mutex<Vec<String>>);

    impl SurfaceHandler<Core> for Leaving {
        fn leaving_session(&self, session_id: &str) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(session_id.to_owned());
        }

        fn dispatch(
            &self,
            _intent: <Core as ProtocolExtension>::Intent,
            _facts: SurfaceFacts<'_>,
            _events: &Sender<Core>,
        ) -> SurfaceEffect {
            SurfaceEffect::default()
        }

        fn publish_template_readiness(&self, _catalog: &[ProviderSection], _events: &Sender<Core>) {
        }

        fn readiness_read(&self) -> ReadinessRead {
            Box::new(|| Ok((Vec::new(), Vec::new())))
        }
    }

    /// A same-session reopen or rebind leaves nothing: only a switch to
    /// another session tells the surface, so a request for the session the
    /// host still serves is never settled.
    #[test]
    fn only_a_switch_to_another_session_tells_the_surface() {
        let handler = Leaving::default();
        leaving_session::<Core>(Some(&handler), "same", "same");
        assert!(
            handler
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        leaving_session::<Core>(Some(&handler), "left", "next");
        assert_eq!(
            *handler
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            ["left"]
        );
    }
}
