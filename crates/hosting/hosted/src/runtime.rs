//! Shared product host for one-shot, JSONL, and interactive attachments.
//!
//! The private host loop owns the durable session record and attachment.
//! An active turn borrows that owner while holding its own control, projection,
//! cancel settlement, and authoritative terminal result.

/// Recovery notice for a completed root whose accepted steering never reached history.
pub const RECOVERED_COMPLETED_STEERING_NOTICE: &str =
    "plak: recovered the completed turn; queued steering was not applied and is not retried";

/// The lead of every definite admission refusal's line, live and projected.
pub(crate) const PROMPT_NOT_ADMITTED: &str = "prompt was not admitted";

/// The clause an abandonment's recovery line appends when it settled
/// accepted steering unapplied.
pub(crate) const STEERING_NOT_APPLIED_CLAUSE: &str =
    "; queued steering was not applied and is not retried";

use crate::CliResult;
use crate::sessions::queue::{drop_last_mutable, edit_mutable, move_mutable, remove_mutable};

use crate::session_home::SessionHome;

fn jobs_transition_guard<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    opened: &crate::agent::AttachedCodingSession,
) -> CliResult<()> {
    let locator = opened.locator();
    if let Some(reason) = config
        .state_roots
        .job_registry()
        .transition_refusal(locator.session_id(), locator.branch_id())
    {
        return Err(crate::CliFailure::new(reason));
    }
    // Injected runtimes may use a private service rather than these roots.
    let count = opened.agent().map_or(0, |agent| {
        agent
            .jobs()
            .iter()
            .filter(|job| job.status == plakous::tools::JobStatus::Running)
            .count()
    });
    if count != 0 {
        return Err(crate::CliFailure::new(format!(
            "{count} background processes are running; stop them first (/ps)"
        )));
    }
    Ok(())
}
use crate::session_store::SessionRecordStore;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::{Duration, Instant};

use plakous::agent::{
    Agent, AgentError, AgentErrorKind, AgentEventKind, CancellationStatus, CommandAckDisposition,
    CommandId, ReasoningEffort, TurnControl, TurnEventError, TurnResult, TurnResultOutcome,
};
use plakous::sessions::{SessionRuntimeBinding, SessionTargetBinding};
use plakous::subscriptions::{NativeProvider, NativeSubscriptions, ResolvedProvider};

use crate::agent::ProductAgentSpec;
use crate::interrupt::{CancelRefusal, CancelSettlement, InterruptWatchState, settle_cancel};
use crate::session_log::SessionLog;
use crate::sessions::{
    self, QueueDrainState as SessionQueueDrainState, QueueItem as SessionQueueItem, SessionRecord,
    TranscriptRow,
};

use admission::{
    accept_draft, commit_session_update, emit_prompt_history, emit_queue_snapshot,
    queue_edit_applied, queue_edit_refused, queue_refusal, reject_draft, reject_stale_queue_action,
};
use events::{
    apply_event, observe_cancelled_tool_activity, observe_terminal_tool_activity,
    resolve_approval_intent, settle_live_thinking,
};
use logging::project_presentation_snapshot;

/// Repository-only access to the exact TUI restore projection.
#[doc(hidden)]
pub fn rehearse_presentation_snapshot(
    transcript: &[TranscriptRow],
) -> plakous_host_protocol::views::presentation::PresentationSnapshot {
    match project_presentation_snapshot::<plakous_host_protocol::Core>(transcript) {
        HostEvent::PresentationSnapshot(snapshot) => snapshot,
        _ => unreachable!("restore projection always produces a snapshot"),
    }
}
pub use resume::note_idle_only;
#[cfg(any(test, feature = "test-support"))]
use startup::observe_test_boundary;
use tree::{emit_tree_snapshot_if_revision_advanced, record_emitted_tree_revision};
use turn::{
    admitted_turn_lifecycle, apply_cancel_settlement, emit_turn_lifecycle, flush_assistant_delta,
    flush_deferred_interrupted_status, observe_pending_cancel_settlement,
    operator_facing_agent_error, reconcile_authoritative_assistant, settle_late_cancel,
    terminal_turn_outcome, turn_finalization_ready, turn_lifecycle, unresolved_turn_lifecycle,
};
use usage::request_subscription_usage_service;

mod admission;
mod agent_transcript;
mod binding_document;
mod child_live;
mod clear;
mod compact;
pub(crate) mod events;
#[cfg(test)]
mod extension_tests;
mod fork;
mod host_loop;
mod info;
pub(crate) mod job_events;
mod logging;
mod models;
pub mod network_watch;
#[cfg(test)]
mod notice_tests;
mod oneshot;
mod projection;
#[cfg(test)]
pub(crate) use projection::probe::view_write as observe_projector_view_write;
#[cfg(test)]
mod fault_store;
mod rebind;
mod reconcile;
#[cfg(test)]
mod recover_tests;
mod resources;
pub(crate) mod result_mail;
mod resume;
pub mod shell;
mod source_release;
mod startup;
mod surface;
#[cfg(any(test, feature = "test-support"))]
pub mod test_kit;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod thinking_settle_tests;
mod tool_facts;
mod tools;
mod topology_state;
mod tree;
mod turn;
pub(crate) use turn::truncation_note;
mod turn_drive;
use turn_drive::{TurnHost, drive_one_turn};
mod usage;

pub use binding_document::{
    RootBindingDocument, RootBindingDraft, RootBindingEdit, RootBindingSaveError,
};
pub use clear::clear_session_rebuild;
pub(crate) use host_loop::publication::RECOVERY_PENDING_NOTE;
pub use info::effort_label;
pub use models::effort_from_config;
pub use models::effort_from_record;
pub use models::native_compact_facet;
pub use models::oauth_model_needs_effort;
pub use models::oauth_provider_for_account;
pub use models::ready_catalog_model_needs_effort;
pub use oneshot::{OneShot, OneShotOutcome, RunBounds};
#[doc(hidden)]
pub use projection::projected_head;
pub use projection::{ProjectedSession, project_session};
#[allow(unused_imports, reason = "reserved for tool-shell reap wiring")]
pub(crate) use resources::sample_children_rusage_cumulative;
pub use resources::sample_process_resources;
pub use source_release::SourceReleases;
pub use startup::HostWorker;
pub use startup::create_seeded_session;
#[cfg(any(test, feature = "test-support"))]
pub use startup::run_injected_host;
pub use startup::spawn_deferred_tui_host;
pub use startup::spawn_deferred_tui_host_with_clear_publication;
pub use startup::spawn_host_worker;
pub use surface::{
    ProfileForkOutcome, Surface, SurfaceEffect, SurfaceFacts, SurfaceHandler, SurfaceMcp,
    SurfacePhase, SurfaceProfileFork, SurfaceRefusal, SurfaceSaved, SurfaceSession,
};
#[cfg(any(test, feature = "test-support"))]
pub use topology_state::install_successor_epoch_headless;
pub use topology_state::{HostRows, TopologyRows, TreeRows};
pub use topology_state::{
    durable_tree_id as durable_topology_tree_id, epoch_rebind_refusal, install_successor_epoch,
    requires_explicit_resume, resume_tree,
};

/// Product read-model bounds. Canonical conversation authority remains in
/// Plakous; these limits only bound Plak's editable queue and UI projections.
const MAX_PENDING_PROMPTS: usize = sessions::MAX_PENDING_PROMPTS;

const MAX_PROMPT_BYTES: usize = sessions::MAX_PROMPT_BYTES;

const MAX_RESUME_CHOICES: usize = 200;

/// The outer bound on the whole durable release. It sits far above the
/// per-child bounds inside it — hub retirement is 2s plus a 250ms detach —
/// so a bounded child error is retained and the parent's release is still
/// attempted rather than raced away by this timeout. Reaching it means
/// cleanup is genuinely incomplete: the failure says so instead of
/// reporting a released locator that never happened.
const HOST_RELEASE_DEADLINE: Duration = Duration::from_secs(25);

pub(crate) use plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT;

/// How often the idle loop re-reads `tree.revision` for child-only changes.
const TREE_OBSERVE_INTERVAL: Duration = Duration::from_millis(200);

/// How long an idle host loop blocks on the control channel before re-running
/// its unsolicited observations (auth events, the tree revision check).
///
/// This is not action latency: `recv_timeout` returns the instant an action
/// arrives. It is purely the cadence of the polls that have no demand behind
/// them, so a 10 ms spin cost 100 wakeups a second on a session doing nothing.
/// It stays well under [`TREE_OBSERVE_INTERVAL`] so that interval still governs
/// the tree check, and well under any human-visible auth latency.
const IDLE_OBSERVE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The observation sender a hosted session publishes through.
///
/// Public because [`SurfaceHandler`] takes one: a product surface answers by
/// publishing, and the port cannot hand it something the product cannot name.
///
/// Observations never write a transcript. A producer separately authors a
/// notice intent through `notice`; the host commits those intents and the
/// projector is the only writer of persisted transcript rows.
#[derive(Clone)]
pub struct Sender<P: plakous_host_protocol::ProtocolExtension = plakous_host_protocol::Core> {
    inner: crate::surface_protocol::HostEventSender<P>,
    notices: Arc<std::sync::Mutex<VecDeque<PendingNoticeIntent>>>,
    session_id: Option<Arc<str>>,
}

/// An authored notice intent the host has not committed yet.
///
/// `session_id` is the session a background publisher was started for; the
/// host's own sender leaves it unset because it only ever publishes for the
/// session it owns at that moment.
pub(crate) struct PendingNoticeIntent {
    pub(crate) session_id: Option<Arc<str>>,
    pub(crate) row: TranscriptRow,
}

/// Authored notice intents the host has not committed yet; the oldest drop past this,
/// so a sender no host drains stays bounded.
const MAX_PENDING_NOTICE_INTENTS: usize = 256;

impl<P: plakous_host_protocol::ProtocolExtension> Sender<P> {
    /// Wrap a bounded observation channel.
    #[must_use]
    pub fn over(inner: crate::surface_protocol::HostEventSender<P>) -> Self {
        Self {
            inner,
            notices: Arc::default(),
            session_id: None,
        }
    }
    /// A clone for a publisher that outlives the host's current step (a
    /// background recovery pass, a source finalization): the notices it keeps
    /// are appended only while the host still owns `session_id`.
    pub(crate) fn pinned_to(&self, session_id: &str) -> Self {
        Self {
            inner: self.inner.clone(),
            notices: Arc::clone(&self.notices),
            session_id: Some(Arc::from(session_id)),
        }
    }
    /// Publish one observation; false when the driver has gone. This is
    /// presentation only, including when the observation carries a row.
    pub fn send(&self, value: HostEvent<P>) -> bool {
        self.inner.send(value).is_ok()
    }

    /// Author a notice intent separately from its live observation. Only
    /// persisted scopes are intents; settings and approval feedback are live.
    pub(crate) fn notice(&self, row: TranscriptRow) -> bool {
        if let TranscriptRow::Notice(notice) = &row
            && notice.scope.persisted()
            && let Ok(mut notices) = self.notices.lock()
        {
            if notices.len() >= MAX_PENDING_NOTICE_INTENTS {
                notices.pop_front();
            }
            notices.push_back(PendingNoticeIntent {
                session_id: self.session_id.clone(),
                row: row.clone(),
            });
        }
        self.inner.send(HostEvent::Row(row)).is_ok()
    }
    /// Notice intents authored since the last call, oldest
    /// first.
    pub(crate) fn take_notice_intents(&self) -> Vec<PendingNoticeIntent> {
        self.notices
            .lock()
            .map(|mut notices| notices.drain(..).collect())
            .unwrap_or_default()
    }
    fn resnapshot_transcript_if_required(&self, session: &SessionRecord) {
        if self.inner.transcript_resnapshot_required() {
            let _ = self.send(logging::project_session_snapshot(session));
        }
    }
}

// Neutral observations and intents carry the driver's typed extension.
pub use crate::surface_protocol::{HostEvent, UserAction};

/// One Ready-catalog native model fact used to construct host target authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiNativeModelFact {
    /// Product account label (`codex`, `grok`, `claude`).
    pub account: String,
    /// Exact catalog model id.
    pub exact_id: String,
    /// Authenticated native tool mode when present.
    pub native_tool_mode: Option<plakous::subscriptions::NativeToolMode>,
    /// Catalog reasoning efforts for this exact id.
    pub reasoning_efforts: Vec<ReasoningEffort>,
    /// Exact catalog processing-tier ids with provider name and description.
    pub service_tiers: Vec<(String, String, String)>,
}

/// Closed product runtime authority. Invalid native/OpenRouter mixtures are
/// unrepresentable; protocol-specific construction remains behind this enum.
pub enum HostTargetAuthority {
    Native {
        provider: NativeProvider,
        resolved: Arc<ResolvedProvider>,
        subscriptions: NativeSubscriptions,
        catalog_candidates: Arc<[TuiNativeModelFact]>,
    },
    Platform {
        provider: crate::binding::PlatformProvider,
        subscriptions: NativeSubscriptions,
    },
    OpenRouter {
        subscriptions: NativeSubscriptions,
        evidence: Option<Box<OpenRouterHostEvidence>>,
    },
    #[cfg(any(test, feature = "test-support"))]
    Injected { provider: NativeProvider },
}

impl HostTargetAuthority {
    const fn native_provider(&self) -> Option<NativeProvider> {
        match self {
            Self::Native { provider, .. } => Some(*provider),
            Self::OpenRouter { .. } | Self::Platform { .. } => None,
            #[cfg(any(test, feature = "test-support"))]
            Self::Injected { provider } => Some(*provider),
        }
    }

    fn native_resolved(&self) -> Option<&Arc<ResolvedProvider>> {
        match self {
            Self::Native { resolved, .. } => Some(resolved),
            Self::OpenRouter { .. } | Self::Platform { .. } => None,
            #[cfg(any(test, feature = "test-support"))]
            Self::Injected { .. } => None,
        }
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "the closed injected test authority intentionally has no production subscriptions"
    )]
    fn native_subscriptions(&self) -> Option<&NativeSubscriptions> {
        match self {
            Self::Native { subscriptions, .. } => Some(subscriptions),
            Self::OpenRouter { subscriptions, .. } | Self::Platform { subscriptions, .. } => {
                Some(subscriptions)
            }
            #[cfg(any(test, feature = "test-support"))]
            Self::Injected { .. } => None,
        }
    }

    const fn is_api(&self) -> bool {
        matches!(self, Self::OpenRouter { .. } | Self::Platform { .. })
    }
}

pub struct OpenRouterHostEvidence {
    literal_model: String,
    wire: crate::binding::ResolvedProtocol,
    evidence: plakous::openrouter::OpenRouterChatModelEvidence,
}

impl OpenRouterHostEvidence {
    fn matches(&self, literal_model: &str, wire: crate::binding::ResolvedProtocol) -> bool {
        self.literal_model == literal_model && self.wire == wire
    }
}

/// Configuration for a hosted multi-turn session.
pub struct HostConfig<P: plakous_host_protocol::ProtocolExtension = plakous_host_protocol::Core> {
    /// The product home this session belongs to, as a fact.
    ///
    /// The runtime persists through `store`, logs through `home`, reads trees
    /// through the state roots and credentials through their port; what is left
    /// is the home itself, which a spawned child states on its spec and the
    /// product's own surfaces resolve their files from.
    pub product_home: PathBuf,
    /// Where this session's event log lives.
    pub home: SessionHome,
    /// The port this session's record is loaded, saved, prepared and published through.
    pub store: Arc<dyn SessionRecordStore>,
    pub state_roots: crate::agent::ProductStateRoots,
    pub session: SessionRecord,
    pub target: HostTargetAuthority,
    pub model_id: String,
    pub effort: Option<ReasoningEffort>,
    pub effort_explicit: bool,
    pub compaction_enabled: bool,
    /// Product `turn.max_model_rounds`, handed to every agent this host builds.
    pub max_model_rounds: usize,
    /// Product `subagents`: whether a fresh root may open a topology hub.
    pub topology_enabled: bool,
    /// Home idle-continuation policy, re-read by interactive attached hosts.
    pub auto_continue: bool,
    /// Whether this face is interactive; JSONL must not enable idle continuation.
    pub interactive: bool,
    /// Re-asks the product whether a topology is admitted *now*, for the
    /// recompiles that must see a change made from another shell. `None` keeps
    /// the answer this attach started with, which is what a session with no
    /// product behind it (a test fixture) wants.
    pub topology_admission: Option<Arc<dyn Fn() -> CliResult<bool> + Send + Sync>>,
    /// The product's `[root] agent` template, when one is configured; the
    /// Session projection names it only while this session runs that tuple.
    pub root_template: Option<crate::binding::RootTemplateFact>,
    /// CLI one-shot attach (`plak compact --yes`) must not start a second paid turn.
    pub attached_one_shot: bool,
    pub cwd: PathBuf,
    /// Exact Ready catalog supplied by the product discovery boundary.
    pub ready_catalog: plakous_host_protocol::views::ReadyCatalog,
    /// The product's own surfaces — settings, the roster, the raw config file.
    /// `None` hosts a session with none, and its extension intents are refused
    /// (`surface::SurfaceRefusal`) rather than silently dropped.
    pub surface: Surface<P>,
    /// One-shot acknowledgement for an exact product resume. The product
    /// record and observation are published only after the canonical runtime
    /// has opened successfully.
    pub resume_publication: Option<Arc<AtomicBool>>,
    pub test_injection: Option<Arc<TestHostInjection>>,
    /// Presentation + provider-resolve timings captured before `run_host_loop`.
    pub ready_path: ReadyPathTiming,
    /// A carried conversation the session's runtime is created with. Taken by
    /// the first runtime construction and valid only for a record that has no
    /// runtime yet (`plak session import`).
    pub seed_history: Option<plakous::sessions::PortableSessionHistory>,
    /// Attachment releases this host started; constructed empty
    /// (`SourceReleases::default()`). Switch-back joins that source's release;
    /// host exit joins every outstanding release under one bounded deadline.
    pub source_releases: SourceReleases,
}

impl<P: plakous_host_protocol::ProtocolExtension> HostConfig<P> {
    pub(crate) fn prompt_host_facts(&self) -> plakous_host_protocol::PromptHostFacts {
        plakous_host_protocol::PromptHostFacts {
            host_wakes: self.attached_one_shot || (self.interactive && self.auto_continue),
        }
    }

    /// The durable rows this session's topology questions read: the tree store
    /// the product supplied to the state roots, and the session-record port.
    pub(crate) fn topology_rows(&self) -> HostRows<'_> {
        HostRows::new(&self.state_roots, self.store.as_ref())
    }

    /// Hello/JSONL report the resolved path, never `plakous-default`.
    #[must_use]
    pub fn resolved_composition_label(&self) -> &'static str {
        crate::agent::effective_presentation_label(
            self.session.composition,
            self.target.native_provider(),
            self.target
                .native_resolved()
                .and_then(|resolved| resolved.native_tool_mode()),
        )
    }

    /// `[roster] subagents` as the product states it *now*.
    ///
    /// [`Self::topology_enabled`] is the answer this attach started with. A
    /// recompile that builds a fresh spec — `/clear`, rebind, plan — asks
    /// again, so a `plak config set orchestration.subagents …` from another
    /// shell is in force for the next one, and a product that cannot answer
    /// refuses the recompile. The re-ask is [`HostConfig::topology_admission`], supplied
    /// by the binary: the loop must not know that the answer is a TOML file.
    ///
    /// # Errors
    /// Returns the product's named configuration error when it cannot answer.
    pub fn live_topology_enabled(&self) -> CliResult<bool> {
        self.topology_admission
            .as_ref()
            .map_or(Ok(self.topology_enabled), |probe| probe())
    }
}

/// Wall-clock phases before the host loop opens the canonical session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReadyPathTiming {
    /// `tui_presentation_bundle` (persisted catalogs, no live GET).
    pub presentation_ms: u64,
    /// `build_host_config` including Codex/Grok attach catalog HTTP.
    pub resolve_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProcessResources {
    rss_kb: u64,
    peak_rss_kb: u64,
    threads: u64,
    fds: u64,
}

/// Content-free process resource snapshot for telemetry (Linux + macOS).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessResourceSnapshot {
    pub rss_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub user_cpu: Duration,
    pub system_cpu: Duration,
    pub threads: Option<u64>,
    pub fds: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestRuntimeRole {
    Primary,
    Compact,
    Rebind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostLifecyclePhase {
    ClearResolveAuthority,
    ClearBuildCandidate,
    ClearPublishCandidate,
    ClearActivate,
    RebindResolveTarget,
    RebindPrepareRuntime,
    RebindPublishIntent,
    RebindCanonicalTransition,
    RebindOpenTarget,
    RebindActivate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestHostWorkflow {
    RootDelivery,
    SendNow,
    Fork,
    Rebind,
    Compact,
    /// A switched-away attachment, after its runtime release.
    SourceRelease,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestHostBoundary {
    Prepared,
    RuntimeReturned,
    ProductPublished,
    ProductCommitted,
    TerminalReady,
}

#[cfg(any(test, feature = "test-support"))]
pub const COMPLETION_WINS_QUIT_PROMPT: &str = "complete while quit is accepted";

pub struct TestHostInjection {
    pub runtime:
        Arc<dyn Fn(TestRuntimeRole) -> Result<SessionRuntimeBinding, String> + Send + Sync>,
    pub rebind_target: SessionTargetBinding,
    pub observe: Arc<dyn Fn(TestHostWorkflow, TestHostBoundary) + Send + Sync>,
    /// A child's runtime binding, by agent id and workspace path.
    ///
    /// `runtime` covers the root only, because `startup` is the only place that
    /// builds the root's session; a child's binding is built in
    /// `agent::children::child_session_runtime_binding` from real credentials
    /// and a real catalog. Without this a topology turn could not be driven
    /// offline. `None` leaves that path exactly as it is in production.
    pub child_runtime: Option<Arc<ChildRuntimeFactory>>,
    /// Attach choice each injected rebind build observed.
    ///
    /// The real spec arm applies the same choice to the runtime it builds.
    /// The injected arm (`spec: None`) records it here, because that build
    /// has no provider to attach.
    pub rebind_builds: std::sync::Mutex<Vec<crate::topology_runtime::TopologyHubAttach>>,
}

/// Builds one child's binding with the real binder's scoped construction roots.
pub type ChildRuntimeFactory = dyn Fn(&str, &str, &crate::agent::ProductStateRoots) -> Result<SessionRuntimeBinding, String>
    + Send
    + Sync
    + 'static;

use crate::sessions::queue;

#[cfg(any(test, feature = "test-support"))]
impl TestHostInjection {
    /// Holds attachment release completion for `delay` after runtime release,
    /// so switch-back deterministically exercises the tracked join.
    #[must_use]
    pub fn with_source_release_delay(&self, delay: Duration) -> Arc<Self> {
        let observe = Arc::clone(&self.observe);
        Arc::new(Self {
            runtime: Arc::clone(&self.runtime),
            rebind_target: self.rebind_target.clone(),
            observe: Arc::new(move |workflow, boundary| {
                if workflow == TestHostWorkflow::SourceRelease {
                    thread::sleep(delay);
                }
                observe(workflow, boundary);
            }),
            child_runtime: self.child_runtime.clone(),
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        })
    }
}

/// The last complete Usage projection the host observed, with the instant it
/// was stored.
///
/// Idle `/usage` and Usage/Context tab opens reuse it while it is younger than
/// [`crate::subscription_usage::USAGE_PROJECTION_FRESHNESS`]. A `/usage` raised
/// during a turn cannot re-query the providers; that path serves the same
/// projection with its age rather than reporting `unavailable`.
#[derive(Clone, Default)]
struct HeldUsage {
    inner: Arc<
        std::sync::Mutex<
            Option<(
                plakous_host_protocol::views::presentation::UsageLimitsView,
                Instant,
            )>,
        >,
    >,
}

/// Running session token/call totals for the Usage surfaces.
///
/// Cost is deliberately absent: no provider route reports one, and `$0` for an
/// unknown cost is forbidden. The time recorded here is measured wall time in
/// admitted turn dispatch — the host's own observation, not a provider figure.
#[derive(Clone, Default)]
struct SessionLedger {
    inner: Arc<std::sync::Mutex<SessionLedgerTotals>>,
}

#[derive(Clone, Copy, Default)]
struct SessionLedgerTotals {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    model_calls: u64,
    reporting_model_calls: u64,
    dispatch_millis: u64,
    turns: u64,
}

struct SubscriptionUsageRefresh<P: plakous_host_protocol::ProtocolExtension> {
    requests: Option<std::sync::mpsc::SyncSender<Option<NativeSubscriptions>>>,
    worker: Option<thread::JoinHandle<()>>,
    events: Sender<P>,
    in_flight: Arc<AtomicBool>,
    last_request: std::sync::Mutex<Option<Instant>>,
}

/// How long a teardown waits for the usage worker before abandoning it.
///
/// A subscription-usage refresh is a bounded provider read whose only product
/// is a `HostEvent`; it owns no durable state and no obligation. Joining it
/// unconditionally made quit wait out an in-flight HTTP read, so quit is
/// bounded here and the thread is detached past the deadline.
const SUBSCRIPTION_USAGE_JOIN_DEADLINE: Duration = Duration::from_millis(250);

/// One bounded native allowance refresh across the three OAuth verticals.
///
/// Every Usage view is derived from a single call: the per-provider
/// compatibility lock is held once, so an interactive action never queues
/// behind a second identical read.
#[derive(Clone)]
enum NativeSubscriptionRefresh {
    Read(
        Box<(
            plakous::model::SubscriptionUsageRead,
            plakous::model::SubscriptionUsageRead,
            plakous::model::SubscriptionUsageRead,
        )>,
    ),
    /// This session has no native subscription store, so no read was attempted.
    /// Naming a timeout here would name a condition that never happened.
    NoSubscriptionStore,
    /// The bounded refresh did not return within its deadline.
    TimedOut,
}

/// Pending tool row info stashed from model ToolCall blocks before ToolStarted.
#[derive(Clone, Debug)]
struct PendingToolInfo {
    subject: String,
    /// Expand body (pretty args); capped.
    detail: String,
}

/// Suffix `bounded_text` stamps on a tool observation preview it had to cut
/// (`plakous-agent` `runtime::body`). Its presence is the host's only evidence
/// that what it is counting is a window, not the whole result.
const TOOL_PREVIEW_TRUNCATION_MARKER: &str = "[preview truncated]";

/// Bytes a tool observation preview is bounded to before it reaches the host.
const TOOL_OBSERVATION_PREVIEW_BYTES: usize = 2 * 1024;

/// One bijective pending-subject index. Exact call identity is authoritative;
/// the name queue exists only for observations whose envelope omitted it.
struct ToolSubjectStash {
    by_name: HashMap<String, VecDeque<String>>,
    by_call_id: HashMap<String, (String, PendingToolInfo)>,
}

/// Shell-only host when the OAuth agent cannot attach (NO_AUTH / AUTH_ONLY).
///
/// Handles a profile's Use now and session binding. Product prompts are
/// refused until the TUI reattaches (automatically after Use now, or on an
/// explicit `/attach`) with a resolvable account+model (READY).
pub struct ShellHostConfig<
    P: plakous_host_protocol::ProtocolExtension = plakous_host_protocol::Core,
> {
    /// The product home this shell belongs to, as a fact; the same home the
    /// attached `HostConfig` carries.
    pub product_home: PathBuf,
    /// The product's root binding, which a profile's Use now edits.
    pub binding: Arc<dyn RootBindingDocument>,
    /// The product's native-subscription service, when it has one.
    pub subscriptions: Option<NativeSubscriptions>,
    /// The OpenRouter credential the Usage surface's fourth vertical probes.
    pub openrouter_credentials: Arc<dyn crate::openrouter::OpenRouterCredentials>,
    /// Where this session's event log lives.
    pub home: SessionHome,
    /// The port the shell's session record is saved through.
    pub store: Arc<dyn SessionRecordStore>,
    pub session: SessionRecord,
    pub cwd: PathBuf,
    /// Attach failure reason for status / logs (`missing_defaults` | `effort_required` | `resolve_failed:…`).
    pub reason: String,
    /// Exact Ready catalog supplied by the product discovery boundary.
    pub ready_catalog: plakous_host_protocol::views::ReadyCatalog,
    /// The product surfaces the shell host publishes for; see [`Surface`].
    pub surface: Surface<P>,
}

/// The product's shell host: the non-agent loop that runs when there is
/// nothing to attach to, and the one the deferred host falls back to when the
/// canonical runtime refuses to open.
///
/// The loop itself is product code (settings, the roster, the config file, the
/// binding intents a detached shell accepts), so the hosted startup path takes
/// it as a port rather than calling it by name. The product hands one in with
/// the preparation it already builds.
pub trait ShellHostRunner<P: plakous_host_protocol::ProtocolExtension = plakous_host_protocol::Core>:
    Send
{
    /// What the product contributes to a fallback shell built out of an
    /// attached session's configuration.
    ///
    /// A `SurfaceHandler` is not clonable and the attached host owns the one on
    /// its `HostConfig`, so the fallback needs a second one; the binding
    /// document, the subscription service and the credential are product facts
    /// an attached config does not carry at all.
    fn fallback_facts(&self) -> ShellFallbackFacts<P>;

    /// Run the shell host to completion on the calling thread.
    fn run(
        self: Box<Self>,
        config: ShellHostConfig<P>,
        actions: crate::surface_protocol::UserActionReceiver<P>,
        events: Sender<P>,
        clear_publication: Option<Arc<std::sync::Mutex<Option<String>>>>,
    ) -> CliResult<()>;
}

/// The product half of a fallback shell host.
pub struct ShellFallbackFacts<
    P: plakous_host_protocol::ProtocolExtension = plakous_host_protocol::Core,
> {
    pub surface: Surface<P>,
    pub binding: Arc<dyn RootBindingDocument>,
    pub subscriptions: Option<NativeSubscriptions>,
    pub openrouter_credentials: Arc<dyn crate::openrouter::OpenRouterCredentials>,
}

/// Result of Plak-owned asynchronous discovery and attachment preparation.
pub struct TuiHostPreparation<
    P: plakous_host_protocol::ProtocolExtension = plakous_host_protocol::Core,
> {
    pub providers: Vec<plakous_host_protocol::views::ProviderDef>,
    pub mode: TuiHostMode<P>,
    /// Product-owned background work started once the host is up (the live
    /// catalog refresh); it receives the observation sender the host publishes
    /// through. `None` runs nothing.
    pub catalog_refresh:
        Option<Box<dyn FnOnce(crate::surface_protocol::HostEventSender<P>) + Send>>,
    /// The product's shell host, for the fallback and the detached mode.
    pub shell_host: Box<dyn ShellHostRunner<P>>,
}

/// Exactly one host mode is selected; neither variant creates a second runtime.
#[allow(
    clippy::large_enum_variant,
    reason = "one value, moved once into the host thread; boxing buys nothing"
)]
pub enum TuiHostMode<P: plakous_host_protocol::ProtocolExtension = plakous_host_protocol::Core> {
    Attached(HostConfig<P>),
    Shell(ShellHostConfig<P>),
}

/// What a readiness refresh needs from either host shape (spec 02 §5.5).
trait ProductAuthHost<P: plakous_host_protocol::ProtocolExtension> {
    fn replace_ready_catalog(&mut self, catalog: plakous_host_protocol::views::ReadyCatalog);
    fn ready_catalog(&self) -> &plakous_host_protocol::views::ReadyCatalog;
    /// The product surfaces this host republishes readiness through.
    fn surface(&self) -> Option<&dyn SurfaceHandler<P>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellBindingOutcome {
    Committed,
    Refused,
    RecoveryRequired,
}

#[derive(Clone, Copy)]
enum ExceptionalTerminalProjection<'a> {
    RecoveryRequired(&'a str),
    Interrupted(&'a str),
    UnresolvedError(&'a str),
}

/// Route- and selection-derived Session facts, kept pure so every vertical and
/// every PTC class is testable without a bound runtime.
struct SessionInfoFacts {
    tool_policy: &'static str,
    admitted_options: Vec<String>,
    api_kernel: Option<&'static str>,
    thinking: String,
}

const COMPACT_UNAVAILABLE_DETAIL: &str =
    "compaction did not admit or complete this checkpoint; retry is a new paid operation";

const COMPACT_NOT_NEEDED_DETAIL: &str = "compaction not needed: the selected prefix already fits";

pub(crate) const COMPACT_NO_ELIGIBLE_RANGE_DETAIL: &str =
    "nothing in this conversation can be compacted yet";

pub(crate) const COMPACT_FAILED_DETAIL: &str =
    "compact workflow finished without a compacted checkpoint";

/// The one notice of an admitted compaction a closure abandoned with exact
/// evidence that its summary request was never sent.
pub(crate) const COMPACT_ABANDONED_UNSENT_NOTE: &str = "plak: interrupted compaction was not \
     applied; its summary was not sent and will not be dispatched again; the conversation history \
     is unchanged";

/// The one notice of an admitted compaction a closure abandoned without that
/// evidence.
pub(crate) const COMPACT_ABANDONED_BILLED_NOTE: &str = "plak: interrupted compaction was not \
     applied, may have been billed, and will not be dispatched again; the conversation history is \
     unchanged";

const COMPACT_PROVIDER_QUIET_NOTE: &str =
    "compact didn't finish (the provider went quiet). Context is unchanged. /compact to retry.";

const COMPACT_RECOVERY_REQUIRED_DETAIL: &str =
    "canonical compact requires explicit recovery; prompts remain paused";

const COMPACTING_ACTIVITY_LABEL: &str = "compacting context";

const COMPACT_COMMITTED_DETAIL: &str = "compact workflow committed";

const COMPACT_COMMITTED_STILL_OVERFLOWS_DETAIL: &str = "compact workflow committed; retained projection still overflows; retry is a new paid operation or /clear";

pub(crate) const CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE: &str = "tool-grown context reached the admitted ceiling; the failed root turn is terminal and will not be replayed. /compact may attempt whole-group compaction of this exact checkpoint; it does not guarantee the retained projection will fit";

pub(crate) const CONTEXT_CAPACITY_NATIVE_COMPACT_GUIDANCE: &str = "tool-grown context reached the admitted ceiling; the failed root turn is terminal and will not be replayed. /compact may attempt native compaction of this exact checkpoint; it does not fold the latest user message and does not guarantee the retained projection will fit";

pub(crate) const CONTEXT_CAPACITY_PROTECTED_GUIDANCE: &str = "tool-grown context overflow cannot be compacted; choose a model with a larger admitted ceiling, fork, or start a new session with /clear";

pub(crate) const CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE: &str = "context-capacity recovery is unavailable because its exact source could not be verified; choose a larger model or start a new session";
struct ResolvedRebindTarget {
    route: RebindRoute,
    effort: Option<ReasoningEffort>,
    effort_explicit: bool,
    binding: SessionTargetBinding,
}

/// Where a rebind's target runs.
enum RebindRoute {
    /// A native OAuth subscription. `spec` is absent only for an injected
    /// test target.
    Native {
        provider: NativeProvider,
        resolved: Option<Arc<ResolvedProvider>>,
        spec: Option<Box<ProductAgentSpec>>,
    },
    /// A direct API account or OpenRouter, built from the target's own
    /// product record.
    Api(Box<SessionRecord>),
}

/// One side of a rebind, for naming what actually changed.
#[derive(Clone, Copy)]
struct RebindBefore<'a> {
    account: Option<&'a str>,
    model: &'a str,
    effort: Option<ReasoningEffort>,
}

/// Tree-snapshot push state owned by one attach.
///
/// `durable_topology_tree_id` walks the session's rebind lineage and opens the
/// product store on a hub miss (sqlite open plus WAL pragmas per candidate), so
/// it is resolved once per bound session and cached here. Without the cache
/// every streamed event — every `TextDelta` — paid a store open for any session
/// whose tree is not in the hub, avoiding unnecessary session loads.
///
/// The cache is keyed on the session the id was resolved for, so it invalidates
/// itself at every `config.session` replacement — `/clear`, `/resume`, fork,
/// rebind, a successor epoch — without any replacement site having to remember
/// to reset it. A cache that outlived its session pinned the *previous*
/// session's tree id, and once that tree left the hub every §5.10 live push
/// silently stopped for the rest of the attach.
#[derive(Default)]
struct TreeSnapshotPush {
    child_live: child_live::ChildObservers,
    /// `session.id` the cached `tree_id` was resolved for.
    session_id: Option<String>,
    tree_id: Option<String>,
    last_revision: Option<u64>,
    next_progress_snapshot: Option<Instant>,
    notified_results: std::collections::BTreeSet<String>,
    /// Nodes whose abandoned-turn notice this face has already said.
    ///
    /// The node keeps the fact, so the tree view can show it; this is what
    /// stops the line repeating on every snapshot, the same way
    /// `notified_results` does for child results.
    announced_abandoned_turns: std::collections::BTreeSet<String>,
    /// Last observed lifecycle of each child, keyed by agent id: what the
    /// next tree read compares against to emit milestone rows. `None` until
    /// the first read of this session seeds it without emitting.
    child_lifecycles: Option<std::collections::BTreeMap<String, tree::ChildLifecycle>>,
    /// Questions to the root already shown as `asks` milestones; seeded with
    /// the first read like `child_lifecycles`.
    announced_questions: std::collections::BTreeSet<String>,
}

enum TurnHostOutcome {
    ConsumedDelivery,
    Completed {
        assistant: String,
        finish_reason: plakous::model::FinishReason,
        finish_detail: Option<String>,
        wire_output_limit: Option<std::num::NonZeroU32>,
    },
    ContextCapacity {
        source_checkpoint: plakous::model::CheckpointId,
        recovery: plakous_host_protocol::ContextCapacityRecovery,
        error: String,
    },
    AdmissionRejected {
        error: String,
        stop_drain: bool,
    },
    Failed {
        error: String,
        transient: Option<TransientFailure>,
        /// How the root's await receipts settle at publication.
        exchange: crate::agent::TerminalExchange,
    },
    RecoveryRequired {
        error: String,
    },
    Interrupted {
        assistant: String,
        /// `Closed` for an authoritative cancelled terminal; `Unresolved`
        /// when the cancel left no terminal result.
        exchange: crate::agent::TerminalExchange,
    },
}

struct TransientFailure {
    failed_turn: plakous::model::TurnId,
    gate: crate::agent::transient::ResumeGate,
}

struct DrivenTurn {
    outcome: TurnHostOutcome,
    quit_after_terminal: bool,
}

struct DriveTurnError {
    message: String,
    quit_after_terminal: bool,
}

/// Operator-facing refusal for an approval handle the host never offered.
const APPROVAL_REFUSED_NOTE: &str = "approval refused: no pending decision with that identity";

/// Reasoning bytes buffered for one location whose visibility bound has not
/// arrived.
///
/// On overflow the buffer is dropped and the location becomes non-displayable
/// for the remainder of the block: the host never emits a partial reasoning
/// body it cannot attribute to an admitting bound.
const MAX_PENDING_REASONING_BYTES: usize = 64 * 1024;

/// Per-location reasoning admission state.
#[derive(Debug)]
enum ReasoningLocation {
    /// The provider has not bound this location's visibility yet.
    Pending {
        /// Bytes withheld until an admitting bound arrives. Never flushed on a
        /// non-admitting bound, on overflow, or on turn failure.
        buffered: Vec<u8>,
    },
    /// `Visible` or `Summary`: deltas project straight through.
    Admitted {
        /// Whether any delta has reached the TUI, so a seal is meaningful.
        emitted: bool,
        /// Operator-visible body already sent to the TUI, retained so resume
        /// can restore the block without copying it into model-facing history.
        text: String,
        /// First admitted emit for this location.
        started: Instant,
    },
    /// `Omitted`, `Redacted`, `OpaqueReplay`, an overflow, or an unbound
    /// location at response completion. Nothing is ever emitted.
    Suppressed,
}

/// One sealed, operator-visible reasoning block ready to persist and project.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SealedReasoning {
    location_key: String,
    text: String,
    elapsed_ms: Option<u64>,
}

/// One local-composition cell's observed children.
///
/// The key is the outer marker's own call id, which every child names through
/// `parent_call_id`. Nothing is ever re-parented and no parentage is guessed:
/// a child whose parent the host has not observed stays a top-level row.
#[derive(Debug)]
struct ComposeProjection {
    /// Canonical outer marker name (`exec` / `exec_js` / `exec_py`).
    name: String,
    /// Evaluator cell, once the interpreter has minted one.
    cell_id: Option<String>,
    /// Distinct child call ids observed at all, terminal or not.
    observed_children: std::collections::BTreeSet<String>,
    /// Child call ids observed reaching a terminal state, and whether it failed.
    terminal_children: HashMap<String, bool>,
    /// Highest nested acceptance ordinal the host observed for the cell.
    highest_ordinal: u32,
    /// The marker's own observed terminal row state, once one arrived.
    ///
    /// The outer `exec*` completes when the cell *yields*, and `exec_call_id`
    /// stays bound until the cell terminalises, so children dispatched during a
    /// later `wait*` still name this compose. A yield is therefore not the cell's
    /// end: the row only *paints* terminal once every child the host observed
    /// is itself terminal (see [`ComposeProjection::row_state`]).
    terminal_state: Option<String>,
}

/// One pending operator approval the host offered.
#[derive(Debug)]
struct PendingApproval {
    /// The runtime decision address. It never leaves the host.
    decision: plakous::agent::EffectDecisionRef,
}

/// Per-turn observational projection state owned by the host streaming loop.
///
/// Nothing here is recovery authority: it exists only to
/// turn a droppable observation stream into rows a renderer can present without
/// inventing structure.
#[derive(Debug)]
struct TurnProjection<P: plakous_host_protocol::ProtocolExtension> {
    reasoning: HashMap<(u32, u32), ReasoningLocation>,
    /// Reasoning blocks the in-flight model response sealed, persisted once
    /// that response completes or the turn ends: a replay that discards the
    /// response discards them too.
    unsettled_thinking: Vec<SealedReasoning>,
    /// Length of the turn's assistant text when the last model response
    /// completed; a discarding replay truncates back to it.
    settled_assistant_len: usize,
    cells: HashMap<String, ComposeProjection>,
    /// Call-local process handles for result-derived wait subjects.
    process_waits: HashMap<String, Vec<String>>,
    /// Call-local candidate an `adopt_candidate` names, for its milestone.
    adopt_candidates: HashMap<String, String>,
    /// Call-local row name a start gave a Grok await or interrupt over a
    /// child (`wait_agent`, `interrupt_agent`), kept for its later lines.
    presented_names: HashMap<String, String>,
    /// Scoped command subjects captured from process start rows or the registry.
    process_commands: HashMap<String, String>,
    /// Evaluator cell → the marker call whose Compose row owns it, learnt from
    /// whichever event first carried both. It is the only way a `wait*`
    /// continuation — which names a cell and no parent — can be attributed to
    /// the row its cell already opened. Nothing is guessed: an unknown cell
    /// resolves to no compose.
    marker_by_cell: HashMap<String, String>,
    approvals: HashMap<String, PendingApproval>,
    approval_by_decision: HashMap<plakous::agent::EffectDecisionRef, String>,
    /// Set when the host observed an event gap for this turn; every Compose count
    /// emitted afterwards is labelled partial rather than presented as a total.
    gap_observed: bool,
    /// Set once the host has asked the runtime to cancel this turn.
    ///
    /// A tool that joins the cancel reports a failed observation like any
    /// other, so without this the observed path painted `failed` while the
    /// settlement sweep painted `cancelled` for the same call — the surface saw
    /// one label or the other depending on which path reached the row first.
    cancel_requested: bool,
    /// Terminal tool lines the observed path built during the cancel window and
    /// handed to the cancel path to publish.
    ///
    /// Inside the window the cancel path is the single publisher, so the lines
    /// and the `interrupted` status they precede are in one order whether or
    /// not the tool reported its own terminal in time. The lines are the ones
    /// the observed path built, so nothing the host actually saw — the tool's
    /// own message, the call's wall time — is thrown away.
    cancel_window_terminals: Vec<HostEvent<P>>,
}

impl<P: plakous_host_protocol::ProtocolExtension> Default for TurnProjection<P> {
    fn default() -> Self {
        Self {
            reasoning: HashMap::new(),
            unsettled_thinking: Vec::new(),
            settled_assistant_len: 0,
            cells: HashMap::new(),
            process_waits: HashMap::new(),
            adopt_candidates: HashMap::new(),
            presented_names: HashMap::new(),
            process_commands: HashMap::new(),
            marker_by_cell: HashMap::new(),
            approvals: HashMap::new(),
            approval_by_decision: HashMap::new(),
            gap_observed: false,
            cancel_requested: false,
            cancel_window_terminals: Vec::new(),
        }
    }
}
