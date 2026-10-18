//! Typed driver extensions. Only immutable facts and bounded observations cross this port.

use super::Sender;
use crate::binding::RootTemplateFact;
use crate::sessions::{NoticeScope, TranscriptRow};
use plakous_host_protocol::views::{ProviderDef, ProviderSection};
use plakous_host_protocol::{Core, ProtocolExtension};

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
}

/// Frozen session facts for product-owned identity and workspace presentation.
pub struct SurfaceSession {
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
    /// The MCP tools this session froze at creation; `None` unless the
    /// handler asked for them ([`SurfaceHandler::needs_session_tools`]).
    pub mcp: Option<SurfaceMcp>,
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
    let roster = rows.and_then(|rows| {
        let tree_id = super::topology_state::durable_tree_id(rows, session);
        match crate::topology_runtime::lookup(&tree_id) {
            Some(runtime) => runtime.read(|tree| tree.snapshot.clone()).ok(),
            None => rows.tree(&tree_id).ok().flatten().map(|tree| tree.snapshot),
        }
    });
    SurfaceSession {
        account: session.account.clone(),
        model: session.model.clone(),
        effort: session.effort.clone(),
        fast: session.target_selection == crate::binding::TargetSelection::NativePriority,
        workspace: session.workspace_root.clone(),
        roster,
        mcp: with_tools.then(|| SurfaceMcp::of(session)),
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
    /// A detached session with no conversation took the profile's root.
    Bound,
    /// This session already runs that root with those cards.
    AlreadyActive,
    /// Nothing changed; the reason is operator-facing.
    Refused(String),
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
    /// Receives the outcome.
    pub finish: Box<dyn FnOnce(ProfileForkOutcome) + Send>,
}

impl SurfaceProfileFork {
    /// Ends the request without changing anything.
    pub fn refuse(self, reason: impl Into<String>) {
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

/// The host-owned consequence of an extension intent.
#[derive(Debug, Default)]
pub struct SurfaceEffect {
    /// A completed durable configuration save, if any.
    pub saved: Option<SurfaceSaved>,
    /// Use this profile now.
    pub profile_fork: Option<SurfaceProfileFork>,
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
    /// Re-read credential readiness as product data.
    /// # Errors
    /// Returns a redaction-safe readiness failure.
    fn refresh_readiness(&self) -> Result<(Vec<ProviderDef>, Vec<ProviderSection>), String>;
}

/// Optional driver-owned extension handler.
pub type Surface<P = Core> = Option<Box<dyn SurfaceHandler<P>>>;

pub(super) fn needs_session_tools<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    intent: &P::Intent,
) -> bool {
    surface.is_some_and(|handler| handler.needs_session_tools(intent))
}

pub(super) fn needs_usage<P: ProtocolExtension>(
    surface: Option<&dyn SurfaceHandler<P>>,
    intent: &P::Intent,
) -> bool {
    surface.is_some_and(|handler| handler.needs_usage(intent))
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
