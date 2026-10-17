//! Child-session lifecycle for a topology: binding a spawned child to a real
//! session, starting and reattaching its turns, the turn waiter, and reopening
//! a child after a restart. Implements the topology crate's binder from above.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use plakous::agent::{
    CommandId, RecoveredTurnHandle, TurnResult, TurnResultHandle, TurnResultOutcome,
};
use plakous::model::{ModelId, OutputBoundDisposition, ProcessingTierDisposition, TurnId};
use plakous::sessions::{
    AgentSessionsError, AgentSessionsErrorKind, CreateSessionOutcome, CreateSessionRequest,
    OpenSessionOutcome, OpenSessionRequest, RecoverEffectEvidenceOutcome,
    RecoverEffectEvidenceRequest, RecoveryRequiredReport, SessionEffectEvidence, SessionLocator,
    SessionOperationId, SessionRecoveryAction, SessionRuntimeBinding,
};
use plakous::subscriptions::NativeSubscriptionError;
use plakous::tools::{ToolExecutorId, ToolOutcome};

use crate::agent_tree::SubmitNudgeBudget;
use crate::agent_tree::turn_outcome::TurnOutcome;
use crate::agent_tree_tools::{SpawnBindError, TreeDriver};
use crate::binding::CompositionSelection;
use crate::child_prompts::ChildAssignmentStartKind;
use crate::prompt_inputs::{prompt_agent_tier, prompt_composition};
use crate::topology_runtime::{ChildStartup, ChildStartupStage, LiveRelease};

mod failed_worktree;

/// A child reopen follows one stale-locator answer without a root predicate.
/// A second stale answer is a fault, not another adoption opportunity.
#[doc(hidden)]
#[must_use]
pub fn child_reopen_head(
    error: &AgentSessionsError,
    already_adopted: bool,
) -> Option<SessionLocator> {
    if already_adopted || error.kind() != AgentSessionsErrorKind::StaleLocator {
        return None;
    }
    error.current_locator()
}

use super::{
    ApiTopologyParent, AttachedCodingSession, ProductAgentSpec, SessionToolFence, SpawnParent,
    coding_session_runtime_binding_for, occupancy_session_fence,
    product_subscription_request_limits, require_reconstructed_coding_projection,
    selected_coding_provider_configuration_with_processing_tier, spawn_child_composition,
    spawn_parent_tree_id, spawn_roster_composition, spawn_roster_processing_tier,
};

pub(super) struct ProductChildSessionBinder {
    pub(super) parent: SpawnParent,
}

impl crate::agent_tree_tools::SpawnedSessionBinder for ProductChildSessionBinder {
    fn admit_spawn(
        &self,
        request: &crate::agent_tree_protocol::SpawnAgentRequest,
    ) -> Result<(), crate::agent_tree::TopologyError> {
        let tree_id = spawn_parent_tree_id(&self.parent);
        let Some(runtime) = crate::topology_runtime::lookup(tree_id) else {
            return Ok(());
        };
        runtime.read(|tree| {
            let Some(binding) = tree.snapshot.binding(&request.template) else {
                return Err(crate::agent_tree::TopologyError::new(
                    crate::agent_tree_protocol::TopologyErrorCode::TemplateUnknown,
                    "template is not in this tree epoch's roster",
                ));
            };
            if !binding.account.is_empty()
                && crate::runtime::oauth_provider_for_account(&binding.account).is_none()
                && binding.account != "openrouter"
                && crate::binding::PlatformProvider::for_account(&binding.account).is_none()
            {
                return Err(crate::agent_tree::TopologyError::new(
                    crate::agent_tree_protocol::TopologyErrorCode::SchemaMismatch,
                    "template account is not a supported tree account",
                ));
            }
            let child = crate::tool_plan::TemplateToolSelection::parse(&binding.tools)
                .map(|selection| crate::tool_plan::NodeToolSelection {
                    selection,
                    tool_sections: tree.snapshot.tool_sections.clone(),
                    mcp_tools: None,
                })
                .map_err(|error| error.to_string());
            let extra = child.and_then(|child| self.parent.tool_selection().tools_beyond(&child));
            match extra {
                Ok(extra) if extra.is_empty() => Ok(()),
                Ok(extra) => Err(crate::agent_tree::TopologyError::new(
                    crate::agent_tree_protocol::TopologyErrorCode::TemplateNotSpawnable,
                    format!(
                        "template {} lists tools this agent's frozen tools lack: {}; a child \
                         gets at most its parent's fetch_url, image_gen and mcp tools; no agent \
                         was spawned",
                        request.template,
                        extra.join(", ")
                    ),
                )),
                Err(detail) => Err(crate::agent_tree::TopologyError::new(
                    crate::agent_tree_protocol::TopologyErrorCode::TemplateNotSpawnable,
                    format!(
                        "template {} tools could not be checked against this agent's: {detail}",
                        request.template
                    ),
                )),
            }
        })?
    }

    fn admit_binding<'a>(
        &'a self,
        template: &'a crate::agent_tree_protocol::TemplateName,
        binding: &'a crate::orchestration::RosterBinding,
    ) -> Pin<Box<dyn Future<Output = Result<(), crate::agent_tree::TopologyError>> + Send + 'a>>
    {
        Box::pin(async move {
            let Some(provider) = crate::runtime::oauth_provider_for_account(&binding.account)
            else {
                return Ok(());
            };
            let refusal = |detail: &str| {
                crate::agent_tree::TopologyError::new(
                    crate::agent_tree_protocol::TopologyErrorCode::ModelNotAdmitted,
                    format!(
                        "template {template} binds account {}, which {detail}; no agent was \
                         spawned. Choose another template, or ask the operator to run `plak \
                         login {}`",
                        binding.account, binding.account
                    ),
                )
            };
            let subscriptions = self
                .parent
                .state_roots()
                .native_subscriptions()
                .map_err(|_| refusal("could not be checked"))?;
            match subscriptions.admit_for_dispatch(provider).await {
                Ok(_) => Ok(()),
                Err(NativeSubscriptionError::CredentialUnavailable) => {
                    Err(refusal("is not signed in and Ready"))
                }
                Err(_) => Err(refusal("could not be checked")),
            }
        })
    }

    fn bind(
        &self,
        start: crate::agent_tree_tools::SpawnStart,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<crate::agent_tree_tools::SpawnBound, SpawnBindError>>
                + Send
                + '_,
        >,
    > {
        let parent = self.parent.clone();
        Box::pin(async move {
            let runtime = crate::topology_runtime::lookup(spawn_parent_tree_id(&parent))
                .ok_or_else(|| SpawnBindError::Refused("topology hub is not attached".into()))?;
            let agent_id = start.spawned.agent_id.clone();
            let deadline = tokio::time::Instant::now() + SPAWN_MATERIALIZE_TIMEOUT;
            let startup = tokio::time::timeout_at(deadline, runtime.reserve_startup(&agent_id))
                .await
                .unwrap_or_else(|_| Err("child startup admission deadline exceeded".into()))
                .map_err(|message| SpawnBindError::RecoveryRequired {
                    stage: ChildStartupStage::Admitting,
                    message,
                })?;
            let mut caller = StartupCaller::new(runtime.clone(), agent_id.clone(), startup.clone());
            let worker_startup = startup.clone();
            let worker_runtime = runtime.clone();
            let mut worker = StartupWorker::new(runtime.clone(), agent_id.clone(), startup.clone());
            let task = tokio::spawn(async move {
                let result = async {
                    let created = match Box::pin(create_spawned_child_session(&parent, &start))
                        .await
                    {
                        Ok(created) => created,
                        Err(error) => {
                            worker_runtime
                                .finish_startup(
                                    &agent_id,
                                    &worker_startup,
                                    LiveRelease {
                                        feed: None,
                                        locator: None,
                                        failure: match &error {
                                            SpawnBindError::Refused(_) => None,
                                            SpawnBindError::RecoveryRequired {
                                                message, ..
                                            } => Some(message.clone()),
                                        },
                                    },
                                )
                                .await;
                            return Err(error);
                        }
                    };
                    start_spawned_child(&parent, start, created, &worker_startup, &worker_runtime)
                        .await
                        .map_err(|failure| match failure {
                            ChildStartFailure::Refused(message) => SpawnBindError::Refused(message),
                            ChildStartFailure::Failed(message)
                            | ChildStartFailure::HoldEndedAfterAdmission(message)
                            | ChildStartFailure::SlotOccupied(message) => {
                                SpawnBindError::RecoveryRequired {
                                    stage: worker_startup.stage(),
                                    message,
                                }
                            }
                        })
                }
                .await;
                worker.finish().await;
                result
            });
            match tokio::time::timeout_at(deadline, task).await {
                Ok(Ok(Ok(bound))) => {
                    caller.disarm();
                    Ok(bound)
                }
                Ok(Ok(Err(error))) => {
                    caller.disarm();
                    Err(error)
                }
                Ok(Err(_)) => {
                    caller.disarm();
                    Err(SpawnBindError::RecoveryRequired {
                        stage: startup.stage(),
                        message: "child startup owner failed; session outcome requires recovery"
                            .into(),
                    })
                }
                Err(_) => Err(SpawnBindError::RecoveryRequired {
                    stage: startup.stage(),
                    message: format!(
                        "child startup exceeded {}s; cancellation requested and owner retained for late acknowledgement and release",
                        SPAWN_MATERIALIZE_TIMEOUT.as_secs()
                    ),
                }),
            }
        })
    }
}

// Unlike the caller guard, this guard ends ownership only after the worker
// ends. Exact-reservation checking makes the fallback harmless after transfer.
struct StartupWorker {
    runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: String,
    startup: ChildStartup,
    armed: bool,
}

impl StartupWorker {
    fn new(
        runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
        agent_id: String,
        startup: ChildStartup,
    ) -> Self {
        Self {
            runtime,
            agent_id,
            startup,
            armed: true,
        }
    }

    fn failure() -> LiveRelease {
        LiveRelease {
            feed: None,
            locator: None,
            failure: Some("child startup owner ended without finishing".into()),
        }
    }

    async fn finish(&mut self) {
        self.runtime
            .finish_startup(&self.agent_id, &self.startup, Self::failure())
            .await;
        self.armed = false;
    }
}

impl Drop for StartupWorker {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!(target: "plak.topology", agent_id = %self.agent_id,
                "startup owner dropped without a runtime; cleanup cannot be spawned");
            return;
        };
        let runtime = self.runtime.clone();
        let agent_id = self.agent_id.clone();
        let startup = self.startup.clone();
        handle.spawn(async move {
            runtime
                .finish_startup(&agent_id, &startup, Self::failure())
                .await;
        });
    }
}

// Dropping a nested tool future also requests cancellation. The worker itself
// is never aborted: an acknowledgement may still arrive after its caller left.
struct StartupCaller {
    runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: String,
    startup: ChildStartup,
    armed: bool,
}
impl StartupCaller {
    fn new(
        runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
        agent_id: String,
        startup: ChildStartup,
    ) -> Self {
        Self {
            runtime,
            agent_id,
            startup,
            armed: true,
        }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for StartupCaller {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.startup.request_cancel();
        let _ = self.runtime.record_then_apply(&self.agent_id, |tree| {
            tree.record_host_retirement(
                &self.agent_id,
                format!(
                    "child startup caller left at {:?}; cancellation and release remain owned",
                    self.startup.stage()
                ),
            )
        });
        let runtime = self.runtime.clone();
        let agent_id = self.agent_id.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!(target: "plak.topology", agent_id = %self.agent_id,
                "startup caller dropped without a runtime; cleanup cannot be spawned");
            return;
        };
        handle.spawn(async move {
            let _ = runtime.cancel_live(&agent_id).await;
        });
    }
}

/// Creates the child session and starts its assignment turn in one step.
///
/// The spawn brief is the child's first prompt; nothing else ever delivers it
/// (mail `delegate` is refused). A child that cannot start returns the reason
/// here and the spawn tool fails it closed, so the tree never carries a
/// `running` node with no turn behind it.
/// A child session `create` produced, before its first turn starts.
pub(super) struct CreatedChild {
    attached: AttachedCodingSession,
    tree_id: String,
    start_kind: ChildAssignmentStartKind,
    attach_started: std::time::Instant,
}

/// Runs on the retained startup owner. A caller deadline never drops a
/// potentially admitted session creation operation.
pub(super) async fn create_spawned_child_session(
    parent: &SpawnParent,
    start: &crate::agent_tree_tools::SpawnStart,
) -> Result<CreatedChild, SpawnBindError> {
    let attach_started = std::time::Instant::now();
    let spawned = &start.spawned;
    let tree_id = match parent {
        SpawnParent::Native(spec) => spec
            .topology_tree_id
            .as_deref()
            .unwrap_or(spec.plan_state_authority.as_str())
            .to_owned(),
        SpawnParent::Api(parent) => parent
            .topology_tree_id
            .as_deref()
            .unwrap_or(parent.plan_state_authority.as_str())
            .to_owned(),
    };
    let operation_id = spawn_child_operation_id(&tree_id, &spawned.agent_id)?;
    let fence = occupancy_session_fence(start.tier);
    let account = Some(start.account.as_str()).filter(|account| !account.is_empty());
    let start_kind = ChildAssignmentStartKind::from_composition(prompt_composition(
        spawn_child_start_composition(parent, &spawned.agent_id, account)?,
    ));
    let runtime = child_session_runtime_binding(
        parent,
        &spawned.agent_id,
        &start.path,
        account,
        Some(start.tuple_id.as_str()),
        start.effort.as_deref(),
        false,
        fence,
        // A child being spawned is new; nothing was created under an earlier
        // bound, so it takes the figure its route attests now.
        None,
    )
    .await?;
    let sessions = parent
        .state_roots()
        .runtime_sessions()
        .map_err(|_| "plak runtime service is unavailable".to_owned())?;
    let created = match sessions
        .create(CreateSessionRequest::new(operation_id.clone(), runtime))
        .await
    {
        Ok(created) => created,
        Err(error) => {
            let nothing_created = matches!(sessions.accepted_create(&operation_id).await, Ok(None));
            return Err(child_create_failure(&error, nothing_created));
        }
    };
    match created {
        CreateSessionOutcome::Created { session, .. } => {
            let locator = session.locator();
            let attached = AttachedCodingSession {
                // A child session is new; nothing was created earlier.
                output_bound_rederived: None,
                reachability_probe: None,
                sessions: Arc::clone(&sessions),
                opened: Some(session),
                recovered_turn: None,
                locator,
                topology_owner: None,
            };
            Ok(CreatedChild {
                attached,
                tree_id,
                start_kind,
                attach_started,
            })
        }
        _ => Err(SpawnBindError::RecoveryRequired {
            stage: ChildStartupStage::Creating,
            message: "child session creation requires operation recovery".into(),
        }),
    }
}

/// A child-session create refused for store capacity created nothing, so the
/// spawn is a definite refusal rather than a stranded child. Any other refused
/// create is a definite refusal too when the exact accepted-create lookup
/// proved no session exists (`nothing_created`) and the refusal is one that
/// can settle a never-created spawn
/// ([`ChildReopenErrorKind::settles_never_created`]): construction refused
/// before it wrote anything, so there is no session outcome for recovery to
/// settle. A session the create left behind, a lookup that failed, or a
/// storage or lease refusal leaves the outcome to recovery.
///
/// The proof relies on the lookup running on the same `AgentSessions`
/// instance, after the refused create returned: the instance's store serves
/// one caller's operations in order, so the lookup observes any genesis the
/// create committed. A store reached through a different instance, or a lookup
/// issued concurrently with the create, would prove nothing.
fn child_create_failure(error: &AgentSessionsError, nothing_created: bool) -> SpawnBindError {
    if let Some(message) = crate::storage_pressure::sessions_storage_pressure(
        error,
        &format!("child session could not be created: {error}"),
    ) {
        return SpawnBindError::Refused(message);
    }
    let message = format!(
        "child session could not be created ({:?}): {error}",
        error.kind()
    );
    if nothing_created && ChildReopenErrorKind::of(error).settles_never_created() {
        return SpawnBindError::Refused(message);
    }
    SpawnBindError::RecoveryRequired {
        stage: ChildStartupStage::Creating,
        message,
    }
}

/// Starts the assignment turn on a created child session. Any refusal here
/// releases the session before reporting.
pub(super) async fn start_spawned_child(
    parent: &SpawnParent,
    start: crate::agent_tree_tools::SpawnStart,
    created: CreatedChild,
    startup: &ChildStartup,
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
) -> Result<crate::agent_tree_tools::SpawnBound, ChildStartFailure> {
    let CreatedChild {
        attached,
        tree_id,
        start_kind,
        attach_started,
    } = created;
    let spawned = start.spawned;
    if let Some(session) = attached.opened.as_ref()
        && let Err(error) = require_reconstructed_coding_projection(session)
    {
        release_startup(runtime, &spawned.agent_id, startup, attached).await;
        return Err(error.failure.message().into());
    }
    {
        {
            let session_locator = attached.locator().to_string();
            if let (Some(session_id), Some(tree_id), Some(node_id)) = (
                crate::product_observation::session_id_from_text(&tree_id),
                crate::product_observation::BoundedText64::new(&tree_id),
                crate::product_observation::BoundedText64::new(&spawned.agent_id),
            ) {
                parent.state_roots().emit_product(
                    crate::product_observation::ProductObservation::TreeNodeAttached {
                        at: crate::product_observation::wall_timestamp(),
                        session_id,
                        tree_id,
                        node_id,
                        parent_node_id: None,
                        elapsed: attach_started.elapsed(),
                    },
                );
            }
            let (template, workspace_path) = runtime
                .read(|tree| {
                    let node = tree.node(&spawned.agent_id).ok()?;
                    let template = node
                        .template
                        .as_ref()
                        .map(|template| template.as_str().to_owned())
                        .unwrap_or_else(|| "unknown".to_owned());
                    let workspace_path = tree
                        .workspace(&node.workspace_id)
                        .filter(|row| {
                            row.mode != crate::agent_tree_protocol::WorkspaceMode::Inherit
                        })
                        .map(|_| node.path.display().to_string());
                    Some((template, workspace_path))
                })
                .ok()
                .flatten()
                .unwrap_or_else(|| ("unknown".to_owned(), None));
            let prompt = assignment_start_prompt(
                start_kind,
                &template,
                start.tier,
                &start.description,
                &start.prompt,
                &start.output_schema_json,
                start.turn_token,
                None,
                workspace_path.as_deref(),
            );
            let command_id =
                match CommandId::new(format!("plak:spawn-start:{tree_id}:{}", spawned.agent_id)) {
                    Ok(command_id) => command_id,
                    Err(error) => {
                        release_startup(runtime, &spawned.agent_id, startup, attached).await;
                        return Err(error.to_string().into());
                    }
                };
            let turn_id = start_child_turn_owned(
                runtime,
                &spawned.agent_id,
                attached,
                command_id,
                prompt,
                startup,
            )
            .await?;
            Ok(crate::agent_tree_tools::SpawnBound {
                durably_registered: true,
                session_locator,
                turn_id: Some(turn_id.to_string()),
            })
        }
    }
}

/// Caller ceiling for creation, assignment acknowledgement and registration.
/// The independent owner continues cancellation and release after this bound.
pub(super) const SPAWN_MATERIALIZE_TIMEOUT: Duration = Duration::from_secs(120);

/// The account/model/effort row a spawned node is already bound to.
///
/// Every path that drives a child after its spawn — the mail-delivered
/// assignment Start and the session reopen — must bind what the node was
/// spawned with, not what a caller happens to pass, and both read it from the
/// same three columns.
pub struct ChildBoundBind {
    /// The account the node was spawned with.
    pub account: Option<String>,
    /// The exact tuple it was spawned with.
    pub tuple_id: Option<String>,
    /// The effort it was spawned with.
    pub effort: Option<String>,
}

impl ChildBoundBind {
    /// The bind three columns of one node.
    #[must_use]
    pub fn of(node: &crate::agent_tree::NodeRecord) -> Self {
        Self {
            account: Some(node.account.clone()).filter(|value| !value.is_empty()),
            tuple_id: Some(node.tuple_id.clone()).filter(|value| !value.is_empty()),
            effort: node.effort.clone().filter(|value| !value.is_empty()),
        }
    }
}

/// The composition the child's Start prompt is written for.
///
/// This used to be read off a full `spawned_child_spec`, which is the native
/// OAuth path only: an OpenRouter child (including OpenRouter
/// children of OpenRouter hub roots) was refused here with "spawn child
/// account is not a supported native OAuth account" before
/// `child_session_runtime_binding` ever got to route it to the OpenRouter
/// branch. Building the spec twice also doubled the OAuth status, catalog, and
/// provider-resolution round trips on every spawn. Resolve the one field the
/// Start prompt needs instead, on the same two branches the binding uses.
pub fn spawn_child_start_composition(
    parent: &SpawnParent,
    agent_id: &str,
    account: Option<&str>,
) -> Result<CompositionSelection, String> {
    let tree_id = spawn_parent_tree_id(parent).to_owned();
    let account = account.filter(|value| !value.is_empty());
    if account == Some("openrouter")
        || account
            .and_then(crate::binding::PlatformProvider::for_account)
            .is_some()
    {
        return frozen_openrouter_child_pin(&tree_id, agent_id)
            .map(|(composition, _protocol)| composition);
    }
    let parent_composition = match parent {
        SpawnParent::Native(spec) => spec.composition,
        SpawnParent::Api(parent) => parent.composition,
    };
    let roster_composition = spawn_roster_composition(&tree_id, agent_id);
    let target_account = account
        .map(str::to_owned)
        .ok_or_else(|| "spawn child account is missing".to_string())?;
    let target_provider = crate::runtime::oauth_provider_for_account(target_account.as_str())
        .ok_or_else(|| {
            format!(
                "spawn child account {target_account} is neither a supported native OAuth account \
                 nor openrouter"
            )
        })?;
    spawn_child_composition(parent_composition, target_provider, roster_composition)
}

/// Starts `prompt` on an opened child session and installs it as the live
/// child, with the turn waiter that terminalizes the node when the turn ends.
/// Losing the live slot to a concurrent starter releases this session and
/// reports the occupant's turn instead; a closed hub is an error.
pub(super) async fn release_startup(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    startup: &ChildStartup,
    attached: AttachedCodingSession,
) -> Option<SessionLocator> {
    startup.set_stage(ChildStartupStage::Releasing);
    let sessions = Arc::clone(&attached.sessions);
    let (released, release) = match attached.release_opened().await {
        Ok(locator) => (Some(locator), {
            let (cursor, recorded) = super::live::release_observation(runtime.driver(), locator);
            super::live::released_with_feed(&sessions, locator, None, cursor, recorded).await
        }),
        Err(error) => (
            None,
            LiveRelease {
                feed: None,
                locator: None,
                failure: Some(format!("child startup release failed: {error}")),
            },
        ),
    };
    runtime.finish_startup(agent_id, startup, release).await;
    released
}

/// Why a created child's assignment did not start.
pub(super) enum ChildStartFailure {
    /// Storage pressure refused the start before anything ran, and the child
    /// session was released and deleted: a definite spawn refusal.
    Refused(String),
    /// Anything else; the child's startup outcome needs recovery.
    Failed(String),
    /// A reopen wake's hold ended after the runtime admitted its Start, so
    /// the turn was not bound and its session was released. Its batch is
    /// already in the session's history.
    HoldEndedAfterAdmission(String),
    /// Another turn or startup held the child's live slot when this Start
    /// reserved it. Nothing was reserved, the opened attachment was
    /// released, and nothing is at fault.
    SlotOccupied(String),
}

impl From<String> for ChildStartFailure {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for ChildStartFailure {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

impl From<ChildStartFailure> for String {
    fn from(failure: ChildStartFailure) -> Self {
        match failure {
            ChildStartFailure::Refused(message)
            | ChildStartFailure::Failed(message)
            | ChildStartFailure::HoldEndedAfterAdmission(message)
            | ChildStartFailure::SlotOccupied(message) => message,
        }
    }
}

/// What a child Start must find before it reserves the child and binds its
/// turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StartAdmission {
    /// Any Start the tree already admitted (a spawn, a mail Start).
    Ordinary,
    /// A held child's reopen wake: its reservation and its bind each
    /// re-check the hold, so an interrupt or close that ended it refuses the
    /// Start instead of the reservation writing `running` over it.
    ReopenWake,
}

impl StartAdmission {
    fn check(
        self,
        tree: &crate::agent_tree::ProductAgentTree,
        agent_id: &str,
    ) -> Result<(), String> {
        match self {
            Self::Ordinary => Ok(()),
            Self::ReopenWake if tree.reopen_wake_admissible(agent_id) => Ok(()),
            Self::ReopenWake => Err(super::park::REOPEN_HOLD_ENDED.to_owned()),
        }
    }
}

pub(super) async fn start_child_turn(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: &str,
    attached: AttachedCodingSession,
    command_id: CommandId,
    prompt: impl Into<ChildCommand>,
) -> Result<TurnId, ChildStartFailure> {
    start_child_turn_as(
        runtime,
        agent_id,
        attached,
        command_id,
        prompt,
        StartAdmission::Ordinary,
    )
    .await
}

/// A resubmitted child command whose original the runtime recorded below
/// the feed's retention: it settled consumed and started no turn.
pub(super) const CHILD_COMMAND_CONSUMED: &str = "the child command was recorded below the feed's retention; it settled consumed and started \
     no turn";

pub(super) async fn start_child_turn_as(
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent_id: &str,
    attached: AttachedCodingSession,
    command_id: CommandId,
    prompt: impl Into<ChildCommand>,
    admission: StartAdmission,
) -> Result<TurnId, ChildStartFailure> {
    let prompt = prompt.into();
    let deadline = tokio::time::Instant::now() + SPAWN_MATERIALIZE_TIMEOUT;
    let reserved = tokio::time::timeout_at(
        deadline,
        runtime.reserve_startup_admitted(agent_id, |tree| admission.check(tree, agent_id)),
    )
    .await
    .unwrap_or_else(|_| Err("child startup admission deadline exceeded".into()));
    let startup = match reserved {
        Ok(startup) => startup,
        // The hold ended: nothing was reserved and nothing is at fault.
        Err(error) if error == super::park::REOPEN_HOLD_ENDED => {
            let _ = attached.release_opened().await;
            return Err(ChildStartFailure::Refused(error));
        }
        // Another turn or startup won the slot: it owns the child, so this
        // Start only gives back its own attachment.
        Err(error) if error == crate::topology_runtime::STARTUP_SLOT_OCCUPIED => {
            let _ = attached.release_opened().await;
            return Err(ChildStartFailure::SlotOccupied(error));
        }
        Err(error) => {
            let expected_attempt = runtime
                .read(|tree| {
                    tree.attempt(agent_id)
                        .map(|attempt| attempt.attempt_id.clone())
                })
                .ok()
                .flatten();
            // Release the rejected attachment independently of its caller.
            let owner = runtime.clone();
            let id = agent_id.to_owned();
            tokio::spawn(async move {
                if attached.release_opened().await.is_err() {
                    let _ = owner.record_then_apply(&id, |tree| {
                        tree.record_host_retirement_for_attempt(
                            &id,
                            expected_attempt.as_ref(),
                            "rejected startup attachment release failed".into(),
                        )
                        .map(|_| ())
                    });
                }
            });
            runtime.record_then_apply(agent_id, |tree| {
                tree.record_host_retirement(agent_id, error.clone())
            })?;
            return Err(error.into());
        }
    };
    let caller = StartupCaller::new(runtime.clone(), agent_id.to_owned(), startup.clone());
    let owner = runtime.clone();
    let id = agent_id.to_owned();
    let task = tokio::spawn(async move {
        start_child_turn_owned_with_ack(
            &owner,
            &id,
            attached,
            command_id,
            prompt,
            &startup,
            false,
            admission,
            std::future::ready(()),
        )
        .await
    });
    await_child_start(caller, task, deadline).await
}

async fn await_child_start(
    mut caller: StartupCaller,
    task: tokio::task::JoinHandle<Result<TurnId, ChildStartFailure>>,
    deadline: tokio::time::Instant,
) -> Result<TurnId, ChildStartFailure> {
    let result = match tokio::time::timeout_at(deadline, task).await {
        Ok(Ok(Ok(turn))) => { caller.disarm(); Ok(turn) }
        Ok(Ok(Err(error))) => { caller.disarm(); Err(error) },
        Ok(Err(_)) => Err("child startup owner failed; recovery required".into()),
        Err(_) => Err("child startup acknowledgement deadline exceeded; owner retained for cancellation and release".into()),
    };
    if let Err(ChildStartFailure::Failed(error)) = &result {
        caller
            .runtime
            .record_then_apply(&caller.agent_id, |tree| {
                tree.record_host_retirement(&caller.agent_id, error.clone())
            })
            .map_err(|error| ChildStartFailure::Failed(error.clone()))?;
    }
    result
}

async fn start_child_turn_owned(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    attached: AttachedCodingSession,
    command_id: CommandId,
    prompt: String,
    startup: &ChildStartup,
) -> Result<TurnId, ChildStartFailure> {
    start_child_turn_owned_with_ack(
        runtime,
        agent_id,
        attached,
        command_id,
        prompt,
        startup,
        true,
        StartAdmission::Ordinary,
        std::future::ready(()),
    )
    .await
}

/// What a child Start admits: a fresh prompt the result journal prepares,
/// or a command the journal already holds, retried exactly after a reopen.
pub(super) enum ChildCommand {
    Fresh(String),
    Reopened {
        first: super::result_context::JournaledCommand,
        remaining: std::collections::VecDeque<super::result_context::JournaledCommand>,
    },
}

impl From<String> for ChildCommand {
    fn from(prompt: String) -> Self {
        Self::Fresh(prompt)
    }
}

impl From<&str> for ChildCommand {
    fn from(prompt: &str) -> Self {
        Self::Fresh(prompt.to_owned())
    }
}

/// Wakes and corrective rounds are host instructions, never user input.
fn is_instruction_command(command: &str) -> bool {
    command.starts_with(super::park::WAKE_COMMAND_PREFIX)
        || command.starts_with("plak:submit-harness:")
        || command.starts_with(super::result_context::TRANSIENT_RESUME_PREFIX)
}

// Keeping acknowledgement delivery separate from admission lets the offline
// fixture lose/delay the response while the real runtime owns an admitted turn.
pub(super) async fn start_child_turn_owned_with_ack(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    attached: AttachedCodingSession,
    command_id: CommandId,
    prompt: impl Into<ChildCommand>,
    startup: &ChildStartup,
    delete_on_definite_refusal: bool,
    admission: StartAdmission,
    acknowledged: impl Future<Output = ()> + Send,
) -> Result<TurnId, ChildStartFailure> {
    let publication = runtime.mutate_then_emit(false, |tree| {
        let locator = attached.locator().to_string();
        tree.record_child_session(agent_id, &locator)
            .map_err(|error| error.message)?;
        Ok(())
    });
    if let Err(error) = publication {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err(error.into());
    }
    if let Err(error) =
        super::result_context::project_child_feed(runtime, agent_id, &attached).await
    {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err(error.into());
    }
    if startup.is_cancelled() || runtime.is_closed() {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err("child startup cancelled before assignment admission".into());
    }
    let mut command_id = command_id;
    let mut command_text = command_id.as_str().to_owned();
    let (mut result_context, mut remaining) = match prompt.into() {
        ChildCommand::Fresh(prompt) => {
            match super::result_context::prepare(runtime.driver(), agent_id, &command_text, &prompt)
            {
                Ok(context) => (context, None),
                Err(error) => {
                    release_startup(runtime, agent_id, startup, attached).await;
                    return Err(error.into());
                }
            }
        }
        ChildCommand::Reopened { first, remaining } => (first.context, Some(remaining)),
    };
    startup.set_stage(ChildStartupStage::Starting);
    let Some(agent) = attached.agent() else {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err("opened child has no agent".into());
    };
    let mut acknowledged = Some(acknowledged);
    let submission = loop {
        let command = if is_instruction_command(&command_text) {
            plakous::agent::TurnCommand::StartInstruction {
                command_id,
                text: result_context.prompt.clone(),
            }
        } else {
            plakous::agent::TurnCommand::Start {
                command_id,
                input: result_context.prompt.clone().into(),
            }
        };
        let submission = match agent.command(command).await {
            Ok(submission) => submission,
            Err(error) => {
                let message = format!("child turn start was refused: {error}");
                release_startup(runtime, agent_id, startup, attached).await;
                return Err(message.into());
            }
        };
        if let Some(acknowledged) = acknowledged.take() {
            acknowledged.await;
        }
        // A resubmission whose original was recorded below the feed's retention
        // started nothing now: it settles consumed (spec 03 section 14.1).
        match super::result_context::consumed_below_retention(
            runtime.driver(),
            &attached.sessions,
            attached.locator().session_id(),
            &command_text,
            submission.acknowledgement().disposition(),
            submission
                .acknowledgement()
                .rejection()
                .is_some_and(|rejection| {
                    rejection.kind() == plakous::agent::ControlErrorKind::Reclaimed
                }),
        )
        .await
        {
            Ok(false) => break submission,
            consumed => {
                let settled = consumed.and_then(|_| {
                    super::result_context::acknowledge(
                        runtime.driver(),
                        &command_text,
                        super::result_context::Submission::Consumed,
                    )
                });
                if let Err(error) = settled {
                    release_startup(runtime, agent_id, startup, attached).await;
                    return Err(error.into());
                }
                let Some(pending) = remaining.as_mut() else {
                    release_startup(runtime, agent_id, startup, attached).await;
                    return Err(CHILD_COMMAND_CONSUMED.into());
                };
                // A consumed acknowledgement started nothing. Keep the same
                // startup/attachment and drain the durable retry snapshot.
                let allowed = runtime
                    .read(|tree| admission.check(tree, agent_id))
                    .map_err(|error| error.message)
                    .and_then(std::convert::identity);
                if startup.is_cancelled() || runtime.is_closed() || allowed.is_err() {
                    release_startup(runtime, agent_id, startup, attached).await;
                    return Err(allowed
                        .err()
                        .unwrap_or_else(|| "child startup cancelled before retry".into())
                        .into());
                }
                let next = if let Some(next) = pending.pop_front() {
                    Ok((next.command_id, next.context))
                } else {
                    runtime
                        .read(|tree| super::park::wake_command(tree, agent_id))
                        .map_err(|error| error.message)
                        .and_then(std::convert::identity)
                        .and_then(|id| {
                            super::result_context::prepare(
                                runtime.driver(),
                                agent_id,
                                &id,
                                crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
                            )
                            .map(|context| (id, context))
                        })
                };
                match next.and_then(|(id, context)| {
                    CommandId::new(id.clone())
                        .map(|command| (id, command, context))
                        .map_err(|error| error.to_string())
                }) {
                    Ok((id, command, context)) => {
                        command_text = id;
                        command_id = command;
                        result_context = context;
                    }
                    Err(error) => {
                        release_startup(runtime, agent_id, startup, attached).await;
                        return Err(error.into());
                    }
                }
            }
        }
    };
    if let Some(rejection) = submission.acknowledgement().rejection() {
        let storage_pressure =
            rejection.kind() == plakous::agent::ControlErrorKind::StoragePressure;
        let rejection = rejection.to_string();
        let settled = super::result_context::acknowledge(
            runtime.driver(),
            &command_text,
            super::result_context::Submission::Refused,
        );
        // Claim removal authority before the first cleanup await. A caller
        // cancellation that wins first keeps the uncertain fresh genesis for
        // later GC; a cleanup claim that wins fences all later cancellation
        // requests through release, archive and delete.
        let cleanup_owned = storage_pressure
            && settled.is_ok()
            && delete_on_definite_refusal
            && startup.claim_definite_refusal_cleanup();
        let sessions = Arc::clone(&attached.sessions);
        let released = release_startup(runtime, agent_id, startup, attached).await;
        // Storage pressure refused new work before the turn was admitted, so
        // nothing ran: the spawn is a definite refusal. The just-created
        // session is archived and deleted (never refused for capacity), so
        // the refusal leaves no child session behind.
        if storage_pressure && settled.is_ok() {
            if !delete_on_definite_refusal && released.is_some() {
                return Err(ChildStartFailure::Refused(rejection));
            }
            let deleted = match (cleanup_owned, released) {
                (false, _) => false,
                (true, Some(locator)) => match sessions.archive(locator).await {
                    Ok(report) => sessions
                        .delete(report.session_id(), report.archived_revision())
                        .await
                        .is_ok(),
                    Err(_) => false,
                },
                (true, None) => false,
            };
            if delete_on_definite_refusal && deleted {
                return Err(ChildStartFailure::Refused(rejection));
            }
        }
        return Err(settled
            .err()
            .unwrap_or_else(|| {
                format!("child start was rejected; result mail remains pending: {rejection}")
            })
            .into());
    }
    let Some(turn_id) = submission.acknowledgement().turn_id() else {
        release_startup(runtime, agent_id, startup, attached).await;
        return Err("start produced no turn identity".into());
    };
    startup.set_stage(ChildStartupStage::Registering);
    #[cfg(test)]
    super::park::hooks::after_start_admission(agent_id).await;
    // The turn's waiter registers with the bind, so the bound turn never
    // reads as one nobody will release. Kept outside the lock: it is dropped
    // (unregistered) on every path that never spawns the waiter.
    let mut registration = None;
    if let Err(error) =
        register_admitted_turn(runtime, admission, agent_id, turn_id, &mut registration)
    {
        // Projection precedes release: the construction attachment is not
        // itself evidence that the command was admitted.
        let projection =
            super::result_context::project_child_feed(runtime, agent_id, &attached).await;
        release_startup(runtime, agent_id, startup, attached).await;
        if error == super::park::REOPEN_HOLD_ENDED {
            projection.map_err(ChildStartFailure::from)?;
            super::result_context::acknowledge(
                runtime.driver(),
                &command_text,
                super::result_context::Submission::Recorded,
            )
            .map_err(ChildStartFailure::from)?;
            return Err(ChildStartFailure::HoldEndedAfterAdmission(error));
        }
        return Err(error.into());
    }
    if let Err(error) =
        super::result_context::project_child_feed(runtime, agent_id, &attached).await
    {
        drop(registration.take());
        release_startup(runtime, agent_id, startup, attached).await;
        return Err(error.into());
    }
    // A fresh turn can retire before registration; its admission capability
    // survives retirement. Duplicates have no new turn capability to retain.
    let control = match submission.acknowledgement().disposition() {
        plakous::agent::CommandAckDisposition::Accepted => {
            submission.turn().map(plakous::agent::Turn::control)
        }
        plakous::agent::CommandAckDisposition::Duplicate => agent.turn_control(turn_id),
        _ => None,
    };
    let Some(control) = control else {
        drop(registration.take());
        release_startup(runtime, agent_id, startup, attached).await;
        if submission.acknowledgement().disposition()
            == plakous::agent::CommandAckDisposition::Duplicate
        {
            return Err(
                "duplicate child start has no live turn; result reconciliation required".into(),
            );
        }
        return Err("child turn control is unavailable".into());
    };
    let Some(result) = submission.result().cloned() else {
        drop(registration.take());
        release_startup(runtime, agent_id, startup, attached).await;
        return Err("start produced no result handle".into());
    };
    let feed = ChildFeedSource::of(&attached);
    let child: Box<dyn crate::topology_runtime::LiveChildHandle> = Box::new(
        crate::agent::live::ProductLiveChild::new(attached, turn_id, control),
    );
    startup.set_stage(ChildStartupStage::Registering);
    let accepted = matches!(
        submission.acknowledgement().disposition(),
        plakous::agent::CommandAckDisposition::Accepted
            | plakous::agent::CommandAckDisposition::Duplicate
    );
    let bound = if accepted {
        super::result_context::acknowledge(
            runtime.driver(),
            &command_text,
            super::result_context::Submission::Recorded,
        )
    } else {
        Err("child command was not admitted; result context retained".to_owned())
    };
    if let Err(error) = bound {
        drop(registration.take());
        startup.set_stage(ChildStartupStage::Releasing);
        runtime
            .finish_startup(
                agent_id,
                startup,
                child.cancel_and_release(runtime.driver().clone()).await,
            )
            .await;
        return Err(error.into());
    }
    match runtime
        .occupy_started_child(agent_id.to_owned(), startup, child)
        .await
    {
        Ok(turn_id) => {
            if let Some(registration) = registration {
                spawn_topology_turn_waiter(
                    registration,
                    Arc::clone(&runtime.live),
                    runtime.observer(),
                    Some(feed),
                    result,
                );
            }
            Ok(turn_id)
        }
        Err(loser) => {
            // No waiter will be spawned: release must not wait for its notice.
            drop(registration.take());
            startup.set_stage(ChildStartupStage::Releasing);
            runtime
                .finish_startup(
                    agent_id,
                    startup,
                    loser.cancel_and_release(runtime.driver().clone()).await,
                )
                .await;
            Err("child startup cancelled or hub closed; late acknowledgement reconciled through cancellation and release".into())
        }
    }
}

pub(super) fn spawn_child_operation_id(
    tree_id: &str,
    agent_id: &str,
) -> Result<SessionOperationId, String> {
    // Tact child ids are tree-local (`c1`). The create operation must be too,
    // or a second tree's spawn revalidates the first tree's child session.
    SessionOperationId::new(format!("plak:spawn:{tree_id}:{agent_id}"))
        .map_err(|_| "child spawn operation identity is invalid".to_owned())
}

pub(super) fn child_plan_state_authority(tree_id: &str, agent_id: &str) -> Result<String, String> {
    let authority = format!("{tree_id}.{agent_id}");
    let identity = format!("plak.session.{authority}");
    if identity.len() > 72 {
        return Err("child plan-state authority identity exceeds the portable bound".into());
    }
    Ok(authority)
}

/// Offline injection of a child's runtime binding (test/`test-support` only).
///
/// `TestHostInjection` supplies a fake runtime for the *root* only, because
/// `runtime::startup` builds the root's session and nothing else does. A
/// child's binding is built here, from `NativeSubscriptions` and a
/// `resolve_turn_scoped_provider` call — real credentials against a real
/// catalog — so a topology turn could not be driven offline at all, and the
/// observation-parity oracle's topology scenario had no way to run. This is the
/// missing half of the seam: `runtime::startup` installs a factory for the life
/// of one host loop, and `child_session_runtime_binding` consults it before
/// it resolves anything.
///
/// The factory is process-global, like the topology hub it serves, and scoped
/// by an RAII guard so one test's fake cannot outlive it.
#[cfg(any(test, feature = "test-support"))]
pub mod injected_child_binding {
    use std::sync::{Arc, Mutex, OnceLock};

    use plakous::sessions::SessionRuntimeBinding;

    /// Builds a child with the construction roots selected by the real binder.
    pub type Factory = dyn Fn(&str, &str, &crate::agent::ProductStateRoots) -> Result<SessionRuntimeBinding, String>
        + Send
        + Sync
        + 'static;

    fn slot() -> &'static Mutex<Option<Arc<Factory>>> {
        static SLOT: OnceLock<Mutex<Option<Arc<Factory>>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    /// Installs `factory` until the returned guard is dropped.
    #[must_use]
    pub fn install(factory: Arc<Factory>) -> Guard {
        if let Ok(mut slot) = slot().lock() {
            *slot = Some(factory);
        }
        Guard(())
    }

    pub(super) fn lookup() -> Option<Arc<Factory>> {
        slot().lock().ok().and_then(|slot| slot.clone())
    }

    /// The installed factory, for the seam's own test.
    #[must_use]
    pub fn lookup_for_test() -> Option<Arc<Factory>> {
        lookup()
    }

    /// Removes the installed factory when dropped.
    #[derive(Debug)]
    pub struct Guard(());

    impl Drop for Guard {
        fn drop(&mut self) {
            if let Ok(mut slot) = slot().lock() {
                *slot = None;
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one child bind names its whole exact tuple"
)]
pub(super) async fn child_session_runtime_binding(
    parent: &SpawnParent,
    agent_id: &str,
    path: &str,
    account: Option<&str>,
    tuple_id: Option<&str>,
    effort: Option<&str>,
    opening_durable_session: bool,
    fence: SessionToolFence,
    // The per-request output bound a child being reopened was created with.
    // `None` for a new child. See `ProductOpenRouterSpec::created_output_bound`:
    // a reopened child is compiled with its own created bound for exactly the
    // same reason a reopened root is, because the same prefix, projection and
    // admission gates compare the same identity.
    created_output_bound: Option<OutputBoundDisposition>,
) -> Result<SessionRuntimeBinding, String> {
    // A new child uses current construction inputs, never a parent's reopen pin.
    let mut scoped_parent = parent.clone();
    if !opening_durable_session {
        match &mut scoped_parent {
            SpawnParent::Native(spec) => {
                spec.state_roots = spec.state_roots.clone().without_reopen_artifact();
            }
            SpawnParent::Api(spec) => {
                spec.state_roots = spec.state_roots.clone().without_reopen_artifact();
            }
        }
    }
    let parent = &scoped_parent;
    #[cfg(any(test, feature = "test-support"))]
    if let Some(factory) = injected_child_binding::lookup() {
        let _ = (tuple_id, effort, opening_durable_session, fence);
        return factory(agent_id, path, parent.state_roots());
    }
    let account = account.filter(|value| !value.is_empty());
    if account == Some("openrouter")
        || account
            .and_then(crate::binding::PlatformProvider::for_account)
            .is_some()
    {
        return openrouter_child_runtime_binding(
            created_output_bound,
            parent,
            agent_id,
            path,
            account,
            tuple_id,
            effort,
            opening_durable_session,
            fence,
        )
        .await;
    }
    let child = spawned_child_spec(parent, agent_id, path, account, tuple_id, effort).await?;
    let mut sink = Vec::new();
    coding_session_runtime_binding_for(
        child,
        &mut sink,
        opening_durable_session,
        fence,
        crate::topology_runtime::TopologyHubAttach::LookupOnly,
        &mut None,
    )
    .map_err(|error| error.message().to_owned())
}

#[allow(
    clippy::too_many_arguments,
    reason = "one child bind names its whole exact tuple"
)]
pub(super) async fn openrouter_child_runtime_binding(
    created_output_bound: Option<OutputBoundDisposition>,
    parent: &SpawnParent,
    agent_id: &str,
    path: &str,
    account: Option<&str>,
    tuple_id: Option<&str>,
    effort: Option<&str>,
    opening_durable_session: bool,
    fence: SessionToolFence,
) -> Result<SessionRuntimeBinding, String> {
    let tree_id = spawn_parent_tree_id(parent);
    let account = account.ok_or_else(|| "API child account is missing".to_owned())?;
    let model = tuple_id
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "API child model is missing".to_owned())?;
    let (composition, protocol) = frozen_openrouter_child_pin(tree_id, agent_id)?;
    let target = if account == "openrouter" {
        crate::binding::ApiTarget::OpenRouter {
            wire: crate::binding::WireBinding::resolve(protocol).resolved,
        }
    } else {
        let provider = crate::binding::PlatformProvider::for_account(account)
            .ok_or_else(|| "API child account is unsupported".to_owned())?;
        if protocol != crate::binding::ProtocolSelection::Auto
            && crate::binding::WireBinding::resolve(protocol).resolved != provider.protocol()
        {
            return Err("API child protocol does not match its exact account".into());
        }
        crate::binding::ApiTarget::Platform { provider }
    };
    let child_path = PathBuf::from(path);
    let workspace_root = child_session_workspace_root(
        match parent {
            SpawnParent::Native(spec) => &spec.workspace_root,
            SpawnParent::Api(parent) => &parent.workspace_root,
        },
        &child_path,
    );
    let selected_effort =
        crate::runtime::effort_from_record(effort).map_err(|error| error.message().to_owned())?;
    let child = ApiTopologyParent {
        created_output_bound,
        // The tree host wakes every child on its tracked work (spec 03 §5.2).
        prompt_host_facts: plakous_host_protocol::PromptHostFacts { host_wakes: true },
        product_home: parent.product_home().map(Path::to_path_buf),
        tool_selection: frozen_child_tool_selection(tree_id, agent_id)?,
        workspace_root,
        cwd: child_path,
        plan_state_authority: child_plan_state_authority(tree_id, agent_id)?,
        composition,
        topology_tree_id: Some(tree_id.to_owned()),
        topology_caller_id: Some(agent_id.to_owned()),
        state_roots: parent.state_roots().clone(),
        model: model.to_owned(),
        effort: selected_effort,
        target,
        scheduled_compaction: match parent {
            SpawnParent::Native(spec) => spec.scheduled_compaction,
            SpawnParent::Api(parent) => parent.scheduled_compaction,
        },
        max_model_rounds: match parent {
            SpawnParent::Native(spec) => spec.max_model_rounds,
            SpawnParent::Api(parent) => parent.max_model_rounds,
        },
        topology_enabled: parent.topology_enabled(),
    };
    Box::pin(super::build::api_session_runtime_binding(
        &child,
        opening_durable_session,
        fence,
        None,
    ))
    .await
    .map(|prepared| prepared.runtime)
    .map_err(|error| error.message().to_owned())
}

/// A child's optional tools: its own frozen card's list, never its
/// parent's, with the epoch's frozen tables. A node with no template, or a
/// card outside this epoch, has none.
pub(super) fn frozen_child_tool_selection(
    tree_id: &str,
    agent_id: &str,
) -> Result<crate::tool_plan::NodeToolSelection, String> {
    let Some(runtime) = crate::topology_runtime::lookup(tree_id) else {
        return Ok(crate::tool_plan::NodeToolSelection::default());
    };
    runtime
        .read(|tree| {
            let Some(binding) = tree
                .node(agent_id)
                .ok()
                .and_then(|node| node.template.as_ref())
                .and_then(|template| tree.snapshot.binding(template))
            else {
                return Ok(crate::tool_plan::NodeToolSelection::default());
            };
            let selection = crate::tool_plan::TemplateToolSelection::parse(&binding.tools)
                .map_err(|error| format!("child template {} tools: {error}", binding.template))?;
            Ok(crate::tool_plan::NodeToolSelection {
                selection,
                tool_sections: tree.snapshot.tool_sections.clone(),
                mcp_tools: None,
            })
        })
        .map_err(|error| error.message)?
}

pub(super) fn frozen_openrouter_child_pin(
    tree_id: &str,
    agent_id: &str,
) -> Result<(CompositionSelection, crate::binding::ProtocolSelection), String> {
    let runtime = crate::topology_runtime::lookup(tree_id)
        .ok_or_else(|| "topology hub is not attached; cannot bind OpenRouter child".to_owned())?;
    runtime
        .read(|tree| {
            let node = tree.node(agent_id).map_err(|error| error.message)?;
            let template = node
                .template
                .as_ref()
                .ok_or_else(|| "OpenRouter child has no template".to_owned())?;
            let binding = tree
                .snapshot
                .binding(template)
                .ok_or_else(|| "OpenRouter child template is not in this tree epoch".to_owned())?;
            // A direct API child binds the composition its enrolled row admits.
            if let Some(provider) = crate::binding::PlatformProvider::for_account(&binding.account)
            {
                let composition = crate::platform::api_composition(
                    provider,
                    &node.tuple_id,
                    binding
                        .composition
                        .map_or(CompositionSelection::FollowCatalog, Into::into),
                )?;
                return Ok((
                    composition,
                    binding
                        .protocol
                        .map_or(crate::binding::ProtocolSelection::Auto, Into::into),
                ));
            }
            openrouter_frozen_pin(
                binding.composition.map(Into::into),
                binding.protocol.map(Into::into),
            )
        })
        .map_err(|error| error.message)?
}

pub(super) fn openrouter_frozen_pin(
    composition: Option<CompositionSelection>,
    protocol: Option<crate::binding::ProtocolSelection>,
) -> Result<(CompositionSelection, crate::binding::ProtocolSelection), String> {
    let composition = composition.unwrap_or(CompositionSelection::FollowCatalog);
    if composition == CompositionSelection::NativePtc {
        return Err(format!(
            "OpenRouter class pin: {}",
            crate::binding::OPENROUTER_NATIVE_PTC_REFUSAL
        ));
    }
    Ok((
        composition,
        protocol.unwrap_or(crate::binding::ProtocolSelection::Auto),
    ))
}

pub async fn spawned_child_spec(
    parent: &SpawnParent,
    agent_id: &str,
    path: &str,
    account: Option<&str>,
    tuple_id: Option<&str>,
    effort: Option<&str>,
) -> Result<ProductAgentSpec, String> {
    let (tree_id, workspace_root_base, state_roots, parent_composition) = match parent {
        SpawnParent::Native(spec) => (
            spec.topology_tree_id
                .clone()
                .unwrap_or_else(|| spec.plan_state_authority.clone()),
            spec.workspace_root.clone(),
            spec.state_roots.clone(),
            spec.composition,
        ),
        SpawnParent::Api(parent) => (
            parent
                .topology_tree_id
                .clone()
                .unwrap_or_else(|| parent.plan_state_authority.clone()),
            parent.workspace_root.clone(),
            parent.state_roots.clone(),
            parent.composition,
        ),
    };
    let child_path = PathBuf::from(path);
    let workspace_root = child_session_workspace_root(&workspace_root_base, &child_path);
    let account = account.filter(|value| !value.is_empty());
    let tuple_id = tuple_id.filter(|value| !value.is_empty());
    let effort = effort.filter(|value| !value.is_empty());
    let target_account = account
        .map(str::to_owned)
        .or_else(|| {
            crate::topology_runtime::lookup(&tree_id).and_then(|runtime| {
                runtime
                    .read(|tree| {
                        tree.node(agent_id)
                            .ok()
                            .filter(|node| !node.account.is_empty())
                            .map(|node| node.account.clone())
                    })
                    .ok()
                    .flatten()
            })
        })
        .ok_or_else(|| "spawn child account is missing".to_string())?;
    let target_provider = crate::runtime::oauth_provider_for_account(target_account.as_str())
        .ok_or_else(|| "spawn child account is not a supported native OAuth account".to_string())?;
    let target_model = tuple_id
        .ok_or_else(|| "spawn child model is missing".to_string())?
        .to_owned();
    let (target_effort, effort_explicit) = if let Some(value) = effort {
        let (effort, explicit) = crate::runtime::effort_from_config(Some(value));
        if !explicit {
            return Err("spawn child effort is invalid".into());
        }
        (effort, explicit)
    } else {
        crate::runtime::effort_from_config(None)
    };
    let roster_composition = spawn_roster_composition(&tree_id, agent_id);
    let child_composition =
        spawn_child_composition(parent_composition, target_provider, roster_composition)?;

    let subscriptions = state_roots
        .native_subscriptions()
        .map_err(|failure| failure.message().to_owned())?;
    // One admission rule with the catalog and inference paths: refresh first
    // (a durable xAI Unknown is reclaimed on the stored refresh token), then
    // read the fail-closed status, then admit.
    subscriptions
        .admit_for_dispatch(target_provider)
        .await
        .map_err(|error| match error {
            NativeSubscriptionError::CredentialUnavailable => {
                "target OAuth account is not Ready".to_string()
            }
            _ => "target OAuth account status could not be read".to_string(),
        })?;
    let catalog = subscriptions
        .model_catalog(target_provider)
        .await
        .map_err(|_| "spawn child model catalog could not be read".to_string())?;
    let catalog_entry = catalog
        .models()
        .iter()
        .find(|entry| entry.exact_id() == target_model)
        .ok_or_else(|| "spawn child model is absent from the authenticated catalog".to_string())?;
    let native_tool_mode = catalog_entry.native_tool_mode();
    // the frozen pin carries the template's tier. It is honored
    // only when this exact authenticated row advertises it; otherwise the
    // child fails closed rather than silently binding Standard.
    let processing_tier = match spawn_roster_processing_tier(&tree_id, agent_id) {
        ProcessingTierDisposition::Standard => ProcessingTierDisposition::Standard,
        ProcessingTierDisposition::ProviderNamed(named) => {
            if catalog_entry
                .service_tiers()
                .iter()
                .any(|advertised| advertised.id() == named.as_str())
            {
                ProcessingTierDisposition::ProviderNamed(named)
            } else {
                return Err(
                    "spawn child processing tier is absent from the exact authenticated catalog row"
                        .into(),
                );
            }
        }
    };
    let model_id = ModelId::new(target_model.clone())
        .map_err(|_| "spawn child model identity is invalid".to_string())?;
    let selected = selected_coding_provider_configuration_with_processing_tier(
        target_provider,
        model_id,
        effort_explicit.then_some(target_effort),
        native_tool_mode,
        child_composition,
        processing_tier,
    )
    .map_err(|error| error.message().to_owned())?;
    let limits = product_subscription_request_limits()
        .map_err(|_| "spawn child subscription gate is invalid".to_string())?;
    let resolved = Arc::new(
        subscriptions
            .resolve_turn_scoped_provider(
                selected,
                limits,
                plakous::model::SessionLineage::SpawnedChild,
            )
            .await
            .map_err(|_| "spawn child provider could not be resolved".to_string())?,
    );

    Ok(ProductAgentSpec {
        // The tree host wakes every child on its tracked work (spec 03 §5.2).
        prompt_host_facts: plakous_host_protocol::PromptHostFacts { host_wakes: true },
        product_home: parent.product_home().map(Path::to_path_buf),
        tool_selection: frozen_child_tool_selection(&tree_id, agent_id)?,
        provider: target_provider,
        resolved,
        effort: effort_explicit.then_some(target_effort),
        effort_explicit,
        workspace_root,
        cwd: child_path,
        plan_state_authority: child_plan_state_authority(&tree_id, agent_id)?,
        composition: child_composition,
        topology_tree_id: Some(tree_id),
        topology_caller_id: Some(agent_id.to_owned()),
        state_roots,
        scheduled_compaction: match parent {
            SpawnParent::Native(spec) => spec.scheduled_compaction,
            SpawnParent::Api(openrouter) => openrouter.scheduled_compaction,
        },
        max_model_rounds: match parent {
            SpawnParent::Native(spec) => spec.max_model_rounds,
            SpawnParent::Api(openrouter) => openrouter.max_model_rounds,
        },
        topology_enabled: parent.topology_enabled(),
    })
}

/// Child `WorkspacePolicy` root is that POSIX copy only.
/// Inherit on the caller copy keeps the parent root; a minted/existing distinct
/// copy is fenced to `child_path` so `../` cannot reach parent or siblings.
pub(super) fn child_session_workspace_root(parent_root: &Path, child_path: &Path) -> PathBuf {
    if parent_root == child_path {
        return parent_root.to_path_buf();
    }
    match (
        std::fs::canonicalize(parent_root),
        std::fs::canonicalize(child_path),
    ) {
        (Ok(parent), Ok(child)) if parent == child => parent_root.to_path_buf(),
        _ => child_path.to_path_buf(),
    }
}

/// The child's first prompt: the spawn brief alone, or the brief plus a mail
/// body when the first Start comes from `send_agent_message` to a child whose
/// turn never bound.
/// `pub(crate)` so rendered prompt goldens can emit the child's first Start
/// user turn. Production behaviour is unchanged.
#[allow(
    clippy::too_many_arguments,
    reason = "one Start prompt is assembled from exactly these node facts"
)]
pub(crate) fn assignment_start_prompt(
    start_kind: ChildAssignmentStartKind,
    template: &str,
    tier: crate::agent_tree_protocol::Tier,
    description: &str,
    prompt: &str,
    schema: &str,
    turn_token: u64,
    message: Option<&str>,
    workspace_path: Option<&str>,
) -> String {
    crate::child_prompts::ChildAssignmentStart {
        kind: start_kind,
        template,
        tier: prompt_agent_tier(tier),
        description,
        prompt,
        schema,
        turn_token,
        workspace_path,
        message,
    }
    .render()
}

/// Reminder (1) then force (2) after a Completed turn with no `submit_result`.
/// Never copies assistant text into `output_json`.
pub(super) fn submit_harness_prompt(nudge: u8, turn_token: u64, schema: &str) -> String {
    crate::child_prompts::SubmitNudge::after(nudge).render(turn_token, schema)
}

/// A follow-up that reached an ended turn leaves its mail for the waiter.
pub(super) fn turn_ended_or(error: String) -> crate::agent_tree_tools::MailDeliveryFailure {
    if error == super::live::TURN_ENDED {
        crate::agent_tree_tools::MailDeliveryFailure::TurnEnded
    } else {
        error.into()
    }
}

pub(super) fn mail_command_id(message_id: &str, kind: &str) -> Result<CommandId, String> {
    CommandId::new(format!("plak:mail:{message_id}:{kind}")).map_err(|error| error.to_string())
}

pub(super) struct ProductLiveCanceller {
    pub(super) runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
}

impl crate::agent_tree_tools::LiveTurnCanceller for ProductLiveCanceller {
    fn cancel_agents(
        &self,
        agent_ids: Vec<String>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::agent_tree_tools::LiveCancelOutcome,
                        crate::agent_tree_tools::LiveCancelFailure,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let report = self.runtime.cancel_live_many(&agent_ids).await;
            if report.is_clean() {
                return Ok(crate::agent_tree_tools::LiveCancelOutcome {
                    cancelled_ids: report.cancelled,
                    not_live_ids: report.not_live,
                });
            }
            Err(crate::agent_tree_tools::LiveCancelFailure {
                message: report
                    .first_failure
                    .unwrap_or_else(|| "child turn could not be cancelled".to_owned()),
                failed_ids: report.failed,
                joiner_expired_ids: report.joiner_expired,
                owner_failures: report.owner_failures,
                cancelled_ids: report.cancelled,
                not_live_ids: report.not_live,
            })
        })
    }
}

pub(super) struct ProductMailDeliverer {
    pub(super) parent: SpawnParent,
    pub(super) runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
}

impl crate::agent_tree_tools::MailDeliverer for ProductMailDeliverer {
    fn withdraw_followup(
        &self,
        driver: TreeDriver,
        agent_id: String,
        message_id: String,
        receipt: Option<(String, crate::agent_tree_protocol::TopologyErrorCode)>,
    ) -> Pin<
        Box<dyn Future<Output = Result<(), crate::agent_tree_protocol::TopologyError>> + Send + '_>,
    > {
        Box::pin(async move {
            use crate::agent_tree_protocol::{TopologyError, TopologyErrorCode};
            use crate::topology_runtime::LiveChildHandle as _;
            let refused =
                |message| TopologyError::new(TopologyErrorCode::OperationRecoveryRequired, message);
            let (path, locator) = driver.read(|tree| {
                let node = tree.node(&agent_id)?;
                Ok::<_, TopologyError>((
                    node.path.display().to_string(),
                    node.session_locator.clone(),
                ))
            })??;
            let locator = locator.ok_or_else(|| refused("child session is missing".to_owned()))?;
            let startup = self
                .runtime
                .reserve_startup_admitted(&agent_id, |_| Ok(()))
                .await
                .map_err(refused)?;
            let opened = reopen_spawned_child(
                &self.parent,
                &agent_id,
                &path,
                &locator,
                "followup-withdrawal",
                ChildReopenMode::REJOIN,
            )
            .await;
            let (attached, recovered) = match opened {
                Ok(ChildReopen::Attached { session, recovered }) => (*session, recovered),
                other => {
                    self.runtime
                        .finish_startup(&agent_id, &startup, LiveRelease::default())
                        .await;
                    return Err(refused(match other {
                        Err(error) => error.message,
                        _ => "child session requires recovery".to_owned(),
                    }));
                }
            };
            // Both the startup slot and the exact session lease remain held
            // through the tail read and tree CAS: no Start can race this check.
            let session_id = attached.locator().session_id();
            let result = if recovered.is_some() {
                Err(refused("child session has an admitted turn".to_owned()))
            } else {
                match super::result_context::feed_bounds(&attached.sessions, session_id).await {
                    Ok((_, tail)) => driver.withdraw_unstarted_followup(
                        &agent_id,
                        &message_id,
                        &session_id.to_string(),
                        tail,
                        receipt,
                    ),
                    Err(error) => Err(refused(error)),
                }
            };
            if let Some(recovered) = recovered {
                let child = crate::agent::live::ProductLiveChild::new(
                    attached,
                    recovered.turn_id(),
                    recovered.control(),
                );
                self.runtime
                    .finish_startup(
                        &agent_id,
                        &startup,
                        Box::new(child)
                            .cancel_and_release(self.runtime.driver().clone())
                            .await,
                    )
                    .await;
            } else {
                release_startup(&self.runtime, &agent_id, &startup, attached).await;
            }
            result
        })
    }

    fn deliver(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::agent_tree_tools::MailDeliveryAck,
                        crate::agent_tree_tools::MailDeliveryFailure,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move { self.deliver_mail(request).await })
    }
}

impl ProductMailDeliverer {
    async fn deliver_mail(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<
        crate::agent_tree_tools::MailDeliveryAck,
        crate::agent_tree_tools::MailDeliveryFailure,
    > {
        match request.disposition {
            crate::agent_tree::MailDisposition::Started => self.start_child(request).await,
            crate::agent_tree::MailDisposition::Queued => self.follow_up_child(request).await,
            crate::agent_tree::MailDisposition::Steered => self.steer_child(request).await,
        }
    }

    async fn start_child(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<
        crate::agent_tree_tools::MailDeliveryAck,
        crate::agent_tree_tools::MailDeliveryFailure,
    > {
        if self.runtime.is_closed() {
            return Err("topology hub is shutting down".into());
        }
        // A Started mail was addressed to a child with no turn. A live slot
        // or a bound turn belongs to a turn some other command admitted: the
        // mail never joins it as a Start (that Start would be acknowledged
        // without ever reaching the turn); the send delivers it there as a
        // follow-up instead. Nothing reopens a session whose slot is still
        // being started or released, and a slot another Start takes while
        // this one reopens is not a fault: the send retries once it is vacant.
        {
            let live = self.runtime.live.lock().await;
            if live.get(&request.agent_id).is_some() {
                return Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded);
            }
            if live.contains_key(&request.agent_id) {
                return Err(crate::agent_tree_tools::MailDeliveryFailure::SlotBusy);
            }
        }
        let stored_turn = self
            .runtime
            .read(|tree| {
                Ok::<_, String>(
                    tree.attempt(&request.agent_id)
                        .and_then(|assignment| assignment.turn_id.clone()),
                )
            })
            .map_err(|error| error.message)??;
        if stored_turn.is_some() {
            return Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded);
        }
        let (attached, recovered) = match reopen_spawned_child(
            &self.parent,
            &request.agent_id,
            &request.path,
            &request.session_locator,
            "mail-open",
            ChildReopenMode::REJOIN,
        )
        .await?
        {
            ChildReopen::Attached { session, recovered } => (*session, recovered),
            ChildReopen::RecoveryRequired(_) => {
                return Err("child session reopen requires explicit recovery".into());
            }
        };
        // The live check above already passed. Another Start can take the
        // slot while this one still has the session open, before
        // `reserve_startup_admitted`. A test forces that window here.
        #[cfg(test)]
        super::park::hooks::around_reopen_spawned_child(&request.agent_id).await;
        if let Some(recovered) = recovered {
            return rejoin_recovered_start(
                &self.runtime,
                &request.agent_id,
                &request.message_id,
                attached,
                recovered.turn_id(),
                recovered.control(),
                recovered.result(),
            )
            .await;
        }
        let (template, tier, workspace_path, bind) = self
            .runtime
            .read(|tree| {
                let node = tree
                    .node(&request.agent_id)
                    .map_err(|error| error.message)?;
                let template = node
                    .template
                    .as_ref()
                    .map(|template| template.as_str().to_owned())
                    .unwrap_or_else(|| "unknown".to_owned());
                let workspace_path = tree
                    .workspace(&node.workspace_id)
                    .filter(|row| row.mode != crate::agent_tree_protocol::WorkspaceMode::Inherit)
                    .map(|_| node.path.display().to_string());
                Ok::<_, String>((
                    template,
                    node.tier,
                    workspace_path,
                    ChildBoundBind::of(node),
                ))
            })
            .map_err(|error| error.message)??;
        // The mail-delivered Start reads the composition off the node's own
        // bound row, the same source `create_spawned_child_session` uses for
        // the spawn Start. Deriving it from a full `spawned_child_spec`
        // instead refused every OpenRouter child here with "spawn child
        // account is not a supported native OAuth account":
        // the exact anti-pattern the note above `spawn_child_start_composition`
        // records, left in place on the assignment path.
        let start_kind = ChildAssignmentStartKind::from_composition(prompt_composition(
            spawn_child_start_composition(
                &self.parent,
                &request.agent_id,
                bind.account.as_deref(),
            )?,
        ));
        let prompt = assignment_start_prompt(
            start_kind,
            &template,
            tier,
            &request.description,
            &request.prompt,
            &request.output_schema_json,
            request.turn_token,
            Some(&request.body),
            workspace_path.as_deref(),
        );
        let command_id = mail_command_id(&request.message_id, "start")?;
        let turn_id = start_child_turn(
            &self.runtime,
            &request.agent_id,
            attached,
            command_id,
            prompt,
        )
        .await
        .map_err(|failure| match failure {
            ChildStartFailure::Refused(message) => {
                crate::agent_tree_tools::MailDeliveryFailure::Refused(message)
            }
            ChildStartFailure::SlotOccupied(_) => {
                crate::agent_tree_tools::MailDeliveryFailure::SlotBusy
            }
            ChildStartFailure::Failed(message)
            | ChildStartFailure::HoldEndedAfterAdmission(message) => {
                crate::agent_tree_tools::MailDeliveryFailure::RecoveryRequired(message)
            }
        })?;
        Ok(crate::agent_tree_tools::MailDeliveryAck {
            turn_id: Some(turn_id.to_string()),
        })
    }

    async fn follow_up_child(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<
        crate::agent_tree_tools::MailDeliveryAck,
        crate::agent_tree_tools::MailDeliveryFailure,
    > {
        self.ensure_live(&request).await?;
        let command_id = mail_command_id(&request.message_id, "follow-up")?;
        let context = super::result_context::prepare(
            self.runtime.driver(),
            &request.agent_id,
            command_id.as_str(),
            &request.body,
        )?;
        // Follow-up is on `Agent`, which is not cloneable; the live-map guard
        // stays held for this one child so a concurrent reopen cannot replace it.
        let mut live = self.runtime.live.lock().await;
        let child = live
            .get_mut(&request.agent_id)
            .ok_or_else(|| "child live session is missing after reopen".to_owned())?;
        child
            .follow_up(command_id, context.prompt)
            .await
            .map_err(turn_ended_or)?;
        let turn_id = child.turn_id();
        let cursor = super::live::live_feed_cursor(self.runtime.driver(), &request.agent_id)?;
        let read = child.feed(cursor);
        drop(live);
        if let Some(feed) = read.await? {
            self.runtime.mutate_then_emit(false, |tree| {
                feed.apply(tree, &request.agent_id)
                    .map(|_| ())
                    .map_err(|error| error.message)
            })?;
        }
        Ok(crate::agent_tree_tools::MailDeliveryAck {
            turn_id: Some(turn_id.to_string()),
        })
    }

    async fn steer_child(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<
        crate::agent_tree_tools::MailDeliveryAck,
        crate::agent_tree_tools::MailDeliveryFailure,
    > {
        self.ensure_live(&request).await?;
        let command_id = mail_command_id(&request.message_id, "steer")?;
        let context = super::result_context::prepare(
            self.runtime.driver(),
            &request.agent_id,
            command_id.as_str(),
            &request.body,
        )?;
        let (control, turn_id) = {
            let live = self.runtime.live.lock().await;
            let child = live
                .get(&request.agent_id)
                .ok_or_else(|| "child live session is missing after reopen".to_owned())?;
            (child.turn_control(), child.turn_id())
        };
        let ack = control
            .steer_with_id(command_id, context.prompt)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(rejection) = ack.rejection() {
            if rejection.kind() == plakous::agent::ControlErrorKind::WrongPhase {
                return Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded);
            }
            return Err(rejection.to_string().into());
        }
        let cursor = super::live::live_feed_cursor(self.runtime.driver(), &request.agent_id)?;
        let read = {
            let live = self.runtime.live.lock().await;
            live.get(&request.agent_id).map(|child| child.feed(cursor))
        };
        if let Some(read) = read
            && let Some(feed) = read.await?
        {
            self.runtime.mutate_then_emit(false, |tree| {
                feed.apply(tree, &request.agent_id)
                    .map(|_| ())
                    .map_err(|error| error.message)
            })?;
        }
        Ok(crate::agent_tree_tools::MailDeliveryAck {
            turn_id: Some(turn_id.to_string()),
        })
    }

    async fn ensure_live(
        &self,
        request: &crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<(), crate::agent_tree_tools::MailDeliveryFailure> {
        if self.runtime.is_closed() {
            return Err("topology hub is shutting down".into());
        }
        {
            let live = self.runtime.live.lock().await;
            if live.get(&request.agent_id).is_some() {
                return Ok(());
            }
            // A slot still starting or retiring has no turn to join yet: the
            // send looks again once it changes.
            if live.contains_key(&request.agent_id) {
                return Err(crate::agent_tree_tools::MailDeliveryFailure::SlotBusy);
            }
        }
        let stored_turn = self
            .runtime
            .read(|tree| {
                Ok::<_, String>(
                    tree.attempt(&request.agent_id)
                        .and_then(|assignment| assignment.turn_id.clone()),
                )
            })
            .map_err(|error| error.message)??;
        let Some(stored_turn) = stored_turn else {
            return Err("no live child session; follow-up and steer require a started turn".into());
        };
        let _stored_turn = TurnId::from_str(&stored_turn)
            .map_err(|_| "stored child turn identity is invalid".to_owned())?;
        let (attached, recovered) = match reopen_spawned_child(
            &self.parent,
            &request.agent_id,
            &request.path,
            &request.session_locator,
            "mail-open",
            ChildReopenMode::OWNER,
        )
        .await?
        {
            ChildReopen::Attached { session, recovered } => (*session, recovered),
            ChildReopen::RecoveryRequired(_) => {
                return Err(
                    "child turn could not be rejoined after reopen; no replacement start was admitted"
                        .into(),
                );
            }
        };
        if let Some(recovered) = recovered {
            if !bound_child_turn_matches(&self.runtime, &request.agent_id, recovered.turn_id()) {
                let _ = attached.release_opened().await;
                return Err("recovered child turn does not match the bound assignment".into());
            }
            occupy_recovered_child(&self.runtime, request.agent_id.clone(), attached, recovered)
                .await?;
            return Ok(());
        }
        // Opened + bound turn has no RecoveredTurnHandle. Joining via
        // turn_control would occupy without a waiter and is a restart drain
        // if Start were admitted next. Fail closed; never Start.
        let _ = attached.release_opened().await;
        // An interrupted child's cancelled turn ended before its waiter
        // released it: the send waits for that release, then resumes the
        // child with this mail (spec 03 section 9). A bound turn no waiter
        // watches is released by nobody, so the send fails closed at once
        // instead of waiting out its budget.
        let unwinding = self
            .runtime
            .read(|tree| {
                let interrupted = tree.node(&request.agent_id).is_ok_and(|node| {
                    node.status == crate::agent_tree_protocol::NodeStatus::Interrupted
                });
                let still_bound = tree
                    .attempt(&request.agent_id)
                    .and_then(|attempt| attempt.turn_id.as_deref())
                    == Some(stored_turn.as_str());
                interrupted && (!still_bound || tree.has_turn_waiter(&request.agent_id))
            })
            .unwrap_or(false);
        if unwinding {
            return Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded);
        }
        Err(
            "child turn could not be rejoined after reopen; no replacement start was admitted"
                .into(),
        )
    }
}

/// Removes a finished child from the live map, releases its session, and
/// advances the node's `session_locator` to the released head.
///
/// The locator bound at spawn is the *created* head; every turn moves the
/// branch, so a later `followup` that reopened the spawn-time locator failed
/// `StaleLocator` and poisoned the node. Releasing here is also what the lease
/// needs: the old path dropped the `OpenedSession` without `release`.
pub async fn retire_live_child(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    expected_turn: TurnId,
) {
    if let Err(error) =
        crate::topology_runtime::retire_live(driver, live, agent_id, Some(expected_turn), false)
            .await
    {
        tracing::error!(target: "plak.topology", agent_id, error = %error, "finished child retirement failed");
    }
}

/// Applies one child mutation and settles its effects after releasing the lock.
/// Callers explicitly publish their durable mutation within `work`.
pub(super) fn mutate_child<R>(
    driver: &TreeDriver,
    agent_id: &str,
    work: impl FnOnce(&mut crate::agent_tree::ProductAgentTree) -> R,
) -> Option<R> {
    let (result, effects) = driver.mutate(work).ok()?;
    settle_or_fault(driver, effects, agent_id);
    Some(result)
}

/// Publishes the tree after a turn-waiter mutation. A refused publish — the
/// store's compare-and-swap found a revision another handle wrote, or the
/// store is gone — is never silent: it is logged and recorded as the node's
/// fault, so the canvas shows that this process's copy of the tree has
/// diverged from the store. The in-memory rows are kept as they are; nothing
/// here retries or reloads, because which copy is right is an operator call.
/// Returns whether the publish succeeded.
pub fn persist_or_fault(
    driver: &TreeDriver,
    tree: &mut crate::agent_tree::ProductAgentTree,
    agent_id: &str,
) -> bool {
    let Err(error) = driver.save(tree) else {
        return true;
    };
    tracing::error!(
        target: "plak.topology",
        tree_id = %tree.tree_id,
        agent_id,
        revision = tree.revision,
        stored_revision = tree.stored_revision,
        error = %error.message,
        "tree publish refused; this process's tree has diverged from the store"
    );
    let reason = match tree.fault_reason(agent_id) {
        Some(prior) => format!("{prior}; tree publish refused: {}", error.message),
        None => format!("tree publish refused: {}", error.message),
    };
    tree.record_fault(agent_id, reason);
    false
}

/// Runs a turn-waiter effects batch through the driver. The
/// drain records every effect before a refused publish is reported, so a
/// seal that ran is in the tree even when the store would not take it; the
/// refusal is then surfaced like [`persist_or_fault`].
pub fn settle_or_fault(driver: &TreeDriver, effects: crate::agent_tree::Effects, agent_id: &str) {
    if effects.is_empty() {
        return;
    }
    let Err(error) = driver.settle(effects) else {
        return;
    };
    tracing::error!(
        target: "plak.topology",
        agent_id,
        error = %error.message,
        "tree effects were recorded but their publish was refused"
    );
    mutate_child(driver, agent_id, |tree| {
        let reason = match tree.fault_reason(agent_id) {
            Some(prior) => format!("{prior}; tree publish refused: {}", error.message),
            None => format!("tree publish refused: {}", error.message),
        };
        tree.record_fault(agent_id, reason);
    });
}

#[derive(Clone)]
pub(super) struct ChildTurnIdentity {
    pub(super) attempt: crate::agent_tree_protocol::AttemptId,
    pub(super) turn: TurnId,
}

impl ChildTurnIdentity {
    pub(super) fn still_owns(
        &self,
        tree: &crate::agent_tree::ProductAgentTree,
        agent_id: &str,
    ) -> bool {
        tree.attempt(agent_id).is_some_and(|attempt| {
            attempt.attempt_id == self.attempt
                && tree
                    .owned_turn_id(agent_id)
                    .or(attempt.turn_id.as_deref())
                    .is_none_or(|turn| turn == self.turn.to_string())
        })
    }
}

/// Ends a reopened parked attempt whose `max_turns` is spent, as the live
/// path ends one: its subtree closes, its processes stop, and the ordinary
/// completion records it `blocked`. The caller already released its session.
pub(super) async fn end_spent_attempt(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    terminal: crate::agent_tree::ChildTerminal,
) -> Result<(), String> {
    let driver = runtime.driver();
    let (closed, effects) = driver
        .mutate(|tree| {
            if tree.revision != tree.stored_revision {
                driver.save(tree)?;
            }
            driver.fold_and_commit(tree, |tree| tree.close_live_descendants(agent_id))
        })
        .map_err(|error| error.message)?;
    let closed = closed.map_err(|error| error.message)?;
    for child in &closed {
        crate::topology_runtime::retire_live(driver, &runtime.live, child, None, true)
            .await
            .map_err(|error| error.to_string())?;
    }
    let mut owners = closed;
    owners.push(agent_id.to_owned());
    driver
        .stop_agent_jobs(&owners)
        .await
        .map_err(|error| error.message)?;
    driver.settle(effects).map_err(|error| error.message)?;
    finalize_child_turn(
        driver,
        agent_id,
        None,
        Some(terminal),
        false,
        true,
        runtime.observer().as_ref(),
    );
    Ok(())
}

pub(super) async fn end_attempt_work(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> Result<(), String> {
    let result = async {
        let (result, effects) = driver
            .mutate(|tree| {
                if tree.revision != tree.stored_revision {
                    driver.save(tree)?;
                }
                driver.fold_and_commit(tree, |tree| {
                    if !identity.still_owns(tree, agent_id)
                        || tree.node(agent_id).is_ok_and(|node| {
                            matches!(
                                node.status,
                                crate::agent_tree_protocol::NodeStatus::Interrupted
                                    | crate::agent_tree_protocol::NodeStatus::RecoveryRequired
                            )
                        })
                    {
                        return Ok((Vec::new(), None, false));
                    }
                    let mut closed = Vec::new();
                    let mut failure = None;
                    for child in tree.live_child_ids(agent_id) {
                        match tree.close_agent(
                            agent_id,
                            &crate::agent_tree_protocol::AttemptScopedRequest {
                                agent_id: child,
                                expected_attempt_id: None,
                            },
                            None,
                        ) {
                            Ok(nodes) => closed.extend(nodes.into_iter().map(|(id, _)| id)),
                            Err(error) => {
                                failure.get_or_insert(error.message);
                            }
                        }
                    }
                    Ok((closed, failure, true))
                })
            })
            .map_err(|error| error.message)?;
        let (closed, mut failure, ended) = result.map_err(|error| error.message)?;
        if !ended {
            return driver
                .settle(effects)
                .map(|_| ())
                .map_err(|error| error.message);
        }
        let mut owners = closed.clone();
        owners.push(agent_id.to_owned());
        let mut retirements = tokio::task::JoinSet::new();
        for child in closed {
            let driver = driver.clone();
            let live = Arc::clone(live);
            retirements.spawn(async move {
                crate::topology_runtime::retire_live(&driver, &live, &child, None, true).await
            });
        }
        while let Some(result) = retirements.join_next().await {
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    failure.get_or_insert(error.to_string());
                }
                Err(error) => {
                    failure.get_or_insert(error.to_string());
                }
            }
        }
        if let Err(error) = driver.stop_agent_jobs(&owners).await {
            failure.get_or_insert(error.message);
        }
        if let Err(error) = driver.settle(effects) {
            failure.get_or_insert(error.message);
        }
        failure.map_or(Ok::<_, String>(()), Err)
    }
    .await;
    if let Err(error) = &result {
        retire_live_child(driver, live, agent_id, identity.turn).await;
        mutate_child(driver, agent_id, |tree| {
            let _ = tree
                .record_host_retirement(agent_id, error.clone())
                .and_then(|value| driver.apply_recorded(tree, agent_id).map(|()| value));
            persist_or_fault(driver, tree, agent_id);
        });
    }
    result
}

/// A turn waiter's registration on the tree. It is taken before the turn
/// it watches is bound or occupies the live slot, so a bound turn some
/// waiter will release always reads as watched; the waiter task holds it for
/// its life however it ends (a panic or an aborted task included), and a turn
/// that never gets its waiter drops it. A send tells a bound turn its waiter
/// will still release from one nobody watches by it (spec 03 section 9).
///
/// Dropping it takes the tree lock: never drop one while holding that lock.
pub(super) struct TurnWaiterRegistration {
    driver: TreeDriver,
    agent_id: String,
    attempt: crate::agent_tree_protocol::AttemptId,
}

fn register_admitted_turn(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    admission: StartAdmission,
    agent_id: &str,
    turn_id: TurnId,
    registration: &mut Option<TurnWaiterRegistration>,
) -> Result<(), String> {
    runtime.prepare_then_emit(|tree| {
        admission.check(tree, agent_id)?;
        tree.bind_turn_owner(agent_id, turn_id.to_string());
        *registration = TurnWaiterRegistration::enter(runtime.driver(), tree, agent_id);
        if admission == StartAdmission::ReopenWake {
            tree.unpark_child(agent_id);
        }
        Ok(())
    })
}

impl TurnWaiterRegistration {
    /// Registers a waiter for `agent_id`'s current attempt under the
    /// caller's tree lock; `None` when the child has no attempt.
    pub(super) fn enter(
        driver: &TreeDriver,
        tree: &mut crate::agent_tree::ProductAgentTree,
        agent_id: &str,
    ) -> Option<Self> {
        let attempt = tree.attempt(agent_id)?.attempt_id.clone();
        tree.enter_turn_waiter(agent_id);
        Some(Self {
            driver: driver.clone(),
            agent_id: agent_id.to_owned(),
            attempt,
        })
    }

    /// [`Self::enter`] under its own tree lock.
    pub(super) fn register(driver: &TreeDriver, agent_id: &str) -> Option<Self> {
        let (registration, effects) = driver
            .mutate(|tree| Self::enter(driver, tree, agent_id))
            .ok()?;
        settle_or_fault(driver, effects, agent_id);
        registration
    }
}

impl Drop for TurnWaiterRegistration {
    fn drop(&mut self) {
        let left = self
            .driver
            .mutate(|tree| tree.leave_turn_waiter(&self.agent_id));
        if let Ok(((), effects)) = left {
            settle_or_fault(&self.driver, effects, &self.agent_id);
        }
    }
}

/// The waiter owns the driver it was registered with: its saves and settles
/// go to the store the hub bound, whether or not the hub still lists this
/// tree when the turn ends (a root that quit releases the hub entry while a
/// child turn can still be unwinding).
pub(super) fn spawn_topology_turn_waiter(
    registration: TurnWaiterRegistration,
    live: Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    observer: Option<Arc<dyn crate::agent_tree::TopologyObserver>>,
    feed: Option<ChildFeedSource>,
    result: TurnResultHandle,
) {
    let driver = registration.driver.clone();
    let agent_id = registration.agent_id.clone();
    let attempt = registration.attempt.clone();
    tokio::spawn(async move {
        let _registration = registration;
        let mut pending = Some(result);
        // What the last wake carried, so leftovers cannot wake forever.
        let mut carried = super::park::Arrivals::new();
        loop {
            let Some(handle) = pending.take() else {
                break;
            };
            let completed_turn = handle.turn_id();
            let identity = ChildTurnIdentity {
                attempt: attempt.clone(),
                turn: completed_turn,
            };
            let terminal = handle.result().await;
            // A child has no operator to wait for, so a turn its owner fenced
            // is recovered at once: release the stopped session and reopen it,
            // which abandons the turn exactly as a crash reattach would. The
            // attempt then ends as an ordinary failure the parent can re-task.
            if let Ok(result) = &terminal
                && matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_))
            {
                project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                if driver
                    .read(|tree| identity.still_owns(tree, &agent_id))
                    .unwrap_or(false)
                    && driver
                        .stop_agent_jobs(std::slice::from_ref(&agent_id))
                        .await
                        .is_err()
                {
                    retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                    break;
                }
                // The recycle may wait out the fenced owner's lease, so it
                // never runs under the reattach lock. Under that lock the
                // node is marked recycling before its live entry goes, which a
                // `recover_agent` or a recovery pass treats as a live owner;
                // the lock is retaken to record the outcome and clear the
                // mark, so recovery either settled the attempt first (and
                // this failure report then changes nothing) or starts after
                // it is recorded.
                let runtime = driver
                    .read(|tree| tree.tree_id.clone())
                    .ok()
                    .and_then(|tree_id| crate::topology_runtime::lookup(&tree_id));
                let recycling = match runtime.as_ref() {
                    Some(runtime) => {
                        let _reattach = runtime.lock_reattach().await;
                        Some(runtime.mark_recycling(&agent_id))
                    }
                    None => None,
                };
                retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                let recycled = recycle_fenced_child(&driver, &agent_id).await;
                let reattach = match runtime.as_ref() {
                    Some(runtime) => Some(runtime.lock_reattach().await),
                    None => None,
                };
                let (disposition, message) = match recycled {
                    Some(FencedChildRecycle::Abandoned(report)) => (
                        crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                            kind: crate::agent_tree::TURN_ABANDONED,
                        },
                        crate::runtime::events::abandoned_turn_operator_line(&report).to_owned(),
                    ),
                    Some(FencedChildRecycle::Quiescent) => (
                        crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                            kind: crate::agent_tree::TURN_ABANDONED,
                        },
                        "child session reopened quiescent after local finalization; \
                         the attempt left no result"
                            .to_owned(),
                    ),
                    Some(FencedChildRecycle::QuiescenceUnproved) => (
                        crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                        "child session reopened but quiescence could not be established".to_owned(),
                    ),
                    None => (
                        crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                        "child turn outcome is unknown and its session could not be reopened \
                         to recover it"
                            .to_owned(),
                    ),
                };
                if !matches!(
                    disposition,
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired
                ) && end_attempt_work(&driver, &live, &agent_id, &identity)
                    .await
                    .is_err()
                {
                    break;
                }
                // The reopen abandoned what the fenced turn left uncommitted.
                finalize_child_failure(
                    &driver,
                    &identity,
                    &agent_id,
                    disposition,
                    message,
                    FailedTurnWaits::Settle(result),
                    observer.as_ref(),
                )
                .await;
                // Cleared while the reattach lock is still held.
                drop(recycling);
                drop(reattach);
                break;
            }
            if let Ok(result) = &terminal
                && let Some(resumed) = try_transient_resume(
                    &driver,
                    &live,
                    &agent_id,
                    &identity,
                    feed.as_ref(),
                    result,
                    observer.as_ref(),
                )
                .await
            {
                if let Ok(next) = resumed {
                    pending = Some(next);
                    continue;
                }
                retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                break;
            }
            if let Ok(result) = &terminal
                && matches!(result.outcome(), TurnResultOutcome::Failed(_))
                && end_attempt_work(&driver, &live, &agent_id, &identity)
                    .await
                    .is_err()
            {
                break;
            }
            if let Ok(result) = &terminal
                && finalize_failed_child_result(
                    &driver,
                    &identity,
                    &agent_id,
                    feed.as_ref(),
                    result,
                    observer.as_ref(),
                )
                .await
            {
                retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                break;
            }
            let node_status = driver
                .read(|tree| tree.node(&agent_id).ok().map(|node| node.status))
                .ok()
                .flatten();
            let ending = TurnOutcome::classify(node_status, child_turn_was_cancelled(&terminal));
            // Below this point `cancelled` means only "the host fenced the
            // node with `close_agent`"; the other two cancel shapes have their
            // own arms and break out of the loop.
            let cancelled = ending == TurnOutcome::Closed;
            if ending == TurnOutcome::CancelledOutside {
                if end_attempt_work(&driver, &live, &agent_id, &identity)
                    .await
                    .is_err()
                {
                    break;
                }
                // end the attempt as an ordinary failure so the
                // parent's `wait_agent` settles, sealing and pushing the
                // `result` by the usual rules.
                if terminal.is_ok() {
                    project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                }
                let pending = driver
                    .mutate(|tree| {
                        if !identity.still_owns(tree, &agent_id) {
                            return;
                        }
                        tree.record_fault(
                            &agent_id,
                            "the turn was cancelled outside the host; nothing interrupted it",
                        );
                        // the seal is a subprocess and runs with
                        // the lock released.
                        let published =
                            tree.record_host_cancellation(&agent_id).and_then(|value| {
                                driver.apply_recorded(tree, &agent_id).map(|()| value)
                            });
                        if let Err(error) = published {
                            tree.record_fault(&agent_id, error.message);
                            return;
                        }
                        tree.try_emit_tree_node_detached(
                            observer.as_ref(),
                            &agent_id,
                            crate::agent_tree::NodeDetach::Failed,
                        );
                    })
                    .map(|((), effects)| effects)
                    .unwrap_or_default();
                settle_or_fault(&driver, pending, &agent_id);
                retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                break;
            }
            if ending == TurnOutcome::Interrupted {
                if terminal.is_ok() {
                    project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                }
                finish_interrupted_turn(&driver, &live, &agent_id, &identity, observer.as_ref())
                    .await;
                break;
            }
            match terminal {
                Ok(_) if !cancelled => {
                    let ended = project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                    let (submitted, harness_nudge) = driver
                        .read(|tree| {
                            tree.attempt(&agent_id).map_or(
                                (false, SubmitNudgeBudget::NONE),
                                |assignment| {
                                    (
                                        assignment.output_json.is_some(),
                                        assignment.submit_harness_nudge,
                                    )
                                },
                            )
                        })
                        .unwrap_or((false, SubmitNudgeBudget::NONE));
                    // Spec 03 §5.2: a turn that ends with live work of its own
                    // parks instead of being nudged; its wake is a new turn.
                    let mut turns_spent = false;
                    if !submitted {
                        use super::park::{ParkEnd, TurnEnd};
                        let end = match super::park::turn_end(
                            &driver, &live, &agent_id, &identity, &carried,
                        )
                        .await
                        {
                            TurnEnd::FollowUp(next) => Some(ParkEnd::Woken(next, carried.clone())),
                            TurnEnd::Park => Some(
                                super::park::park_until_wake(
                                    &driver, &live, &agent_id, &identity, &carried,
                                )
                                .await,
                            ),
                            TurnEnd::Interrupted => Some(ParkEnd::Interrupted),
                            TurnEnd::Settle => None,
                            TurnEnd::Spent => {
                                turns_spent = true;
                                None
                            }
                        };
                        match end {
                            None => {}
                            Some(ParkEnd::Woken(next, arrivals)) => {
                                carried = arrivals;
                                pending = Some(next);
                                continue;
                            }
                            Some(ParkEnd::Ended) => break,
                            Some(ParkEnd::Interrupted) => {
                                finish_interrupted_turn(
                                    &driver,
                                    &live,
                                    &agent_id,
                                    &identity,
                                    observer.as_ref(),
                                )
                                .await;
                                break;
                            }
                            Some(ParkEnd::WakeFailed(error)) => {
                                end_failed_wake(
                                    &driver,
                                    &live,
                                    &agent_id,
                                    &identity,
                                    ended.clone(),
                                    &error,
                                    observer.as_ref(),
                                )
                                .await;
                                break;
                            }
                        }
                    }
                    if submitted || turns_spent || harness_nudge.is_exhausted() {
                        if end_attempt_work(&driver, &live, &agent_id, &identity)
                            .await
                            .is_err()
                        {
                            break;
                        }
                        finalize_child_turn(
                            &driver,
                            &agent_id,
                            Some(&identity),
                            ended.clone(),
                            cancelled,
                            true,
                            observer.as_ref(),
                        );
                        retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                        break;
                    }
                    // `is_exhausted` above is the only exit, so the budget
                    // always has a next round here.
                    let next_nudge = harness_nudge.next().unwrap_or(harness_nudge);
                    if let Ok(next) =
                        start_submit_harness_turn(&driver, &live, &agent_id, &identity, next_nudge)
                            .await
                    {
                        pending = Some(next);
                    } else {
                        let reconciliation_required = driver.read(|tree| {
                            tree.attempt(&agent_id).is_some_and(|attempt|
                                attempt.status == crate::agent_tree_protocol::AttemptStatus::RecoveryRequired)
                        }).unwrap_or(false);
                        if reconciliation_required {
                            retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                            break;
                        }
                        let submitted = driver
                            .read(|tree| {
                                tree.attempt(&agent_id)
                                    .is_some_and(|assignment| assignment.output_json.is_some())
                            })
                            .unwrap_or(false);
                        if end_attempt_work(&driver, &live, &agent_id, &identity)
                            .await
                            .is_err()
                        {
                            break;
                        }
                        finalize_child_turn(
                            &driver,
                            &agent_id,
                            Some(&identity),
                            ended.clone(),
                            cancelled,
                            submitted,
                            observer.as_ref(),
                        );
                        retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                        break;
                    }
                }
                Ok(_) => {
                    let ended = project_turn_end(&driver, feed.as_ref(), &agent_id).await;
                    if end_attempt_work(&driver, &live, &agent_id, &identity)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    finalize_child_turn(
                        &driver,
                        &agent_id,
                        Some(&identity),
                        ended.clone(),
                        cancelled,
                        true,
                        observer.as_ref(),
                    );
                    retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                    break;
                }
                Err(error) => {
                    // No authoritative terminal was published. Submitted work
                    // cannot turn this channel failure into completion.
                    finalize_child_failure(
                        &driver,
                        &identity,
                        &agent_id,
                        crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                        format!(
                            "child terminal publication failed ({:?}): {}",
                            error.kind(),
                            error.message()
                        ),
                        FailedTurnWaits::Keep,
                        observer.as_ref(),
                    )
                    .await;
                    retire_live_child(&driver, &live, &agent_id, completed_turn).await;
                    break;
                }
            }
        }
    });
}

/// Spec 03 §9: an interrupt cancels this turn and nothing more. The node
/// stays `Interrupted`, the attempt stays `running`, nothing seals and no
/// `result` is pushed. Releasing the bound turn is what makes the node
/// reusable: the next mail (a steer or a coordinate) starts a fresh turn on
/// the same attempt instead of trying to rejoin a turn that has already
/// ended (the cancelled `Ok(result)` fell through to the
/// submit harness, which found no live session and marked the child
/// `recovery_required` with `session_materialization`). An interrupt that
/// lands after the turn ended, at the park decision or during a wake's
/// admission, ends here too.
async fn finish_interrupted_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    finalize_child_turn(
        driver,
        agent_id,
        Some(identity),
        None,
        true,
        false,
        observer,
    );
    retire_live_child(driver, live, agent_id, identity.turn).await;
}

/// A parked child's wake was not admitted. The attempt ends as the submit
/// harness would end it, unless an interrupt won the race (the attempt stays
/// open) or the start needs reconciliation. While the attempt ends the child
/// stays marked ending, so mail sent meanwhile is refused as terminal rather
/// than starting a turn on a session whose turn the tree no longer binds.
async fn end_failed_wake(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    terminal: Option<crate::agent_tree::ChildTerminal>,
    error: &str,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    tracing::error!(target: "plak.topology", agent_id, error = %error,
        "parked child wake was not admitted; ending the attempt");
    let unpark = || {
        mutate_child(driver, agent_id, |tree| tree.unpark_child(agent_id));
    };
    let reconciliation_required = driver
        .read(|tree| {
            tree.attempt(agent_id).is_some_and(|attempt| {
                attempt.status == crate::agent_tree_protocol::AttemptStatus::RecoveryRequired
            })
        })
        .unwrap_or(false);
    if reconciliation_required {
        unpark();
        retire_live_child(driver, live, agent_id, identity.turn).await;
        return;
    }
    let ending = mutate_child(driver, agent_id, |tree| {
        identity.still_owns(tree, agent_id) && tree.mark_park_ending(agent_id)
    })
    .unwrap_or(false);
    if !ending {
        let interrupted = driver
            .read(|tree| {
                identity.still_owns(tree, agent_id)
                    && tree.node(agent_id).is_ok_and(|node| {
                        node.status == crate::agent_tree_protocol::NodeStatus::Interrupted
                    })
            })
            .unwrap_or(false);
        unpark();
        if interrupted {
            finish_interrupted_turn(driver, live, agent_id, identity, observer).await;
        } else {
            retire_live_child(driver, live, agent_id, identity.turn).await;
        }
        return;
    }
    mutate_child(driver, agent_id, |tree| {
        let settled = tree
            .record_failed_wake_mail(agent_id, &identity.turn.to_string(), error)
            .and_then(|()| tree.project_mail_decisions(agent_id));
        if let Err(error) = settled {
            tree.record_fault(agent_id, error.message);
        }
        persist_or_fault(driver, tree, agent_id);
    });
    #[cfg(test)]
    super::park::hooks::before_failed_wake_end(agent_id).await;
    if end_attempt_work(driver, live, agent_id, identity)
        .await
        .is_err()
    {
        unpark();
        return;
    }
    finalize_child_turn(
        driver,
        agent_id,
        Some(identity),
        terminal,
        false,
        true,
        observer,
    );
    unpark();
    retire_live_child(driver, live, agent_id, identity.turn).await;
}

enum FencedChildRecycle {
    Abandoned(plakous::agent::AbandonedTurnReport),
    Quiescent,
    QuiescenceUnproved,
}

/// Reopens a child session whose live owner fenced its turn, so the reopen
/// abandons that turn or closes local finalization debt. `None` is a refusal.
async fn recycle_fenced_child(driver: &TreeDriver, agent_id: &str) -> Option<FencedChildRecycle> {
    let tree_id = driver.read(|tree| tree.tree_id.clone()).ok()?;
    let runtime = crate::topology_runtime::lookup(&tree_id)?;
    let parent = super::build::rejoin_parent(&runtime)?;
    let (locator, path) = driver
        .read(|tree| {
            tree.node(agent_id).ok().and_then(|node| {
                Some((
                    node.session_locator.clone()?,
                    node.path.display().to_string(),
                ))
            })
        })
        .ok()??;
    let Ok(ChildReopen::Attached { session, .. }) = reopen_spawned_child(
        &parent,
        agent_id,
        &path,
        &locator,
        "recover",
        ChildReopenMode::OWNER,
    )
    .await
    else {
        return None;
    };
    let report = session
        .agent()
        .and_then(plakous::agent::Agent::abandoned_turn)
        .cloned();
    let recycled = if let Some(report) = report {
        FencedChildRecycle::Abandoned(report)
    } else {
        match session.sessions.head_is_quiescent(session.locator()).await {
            Ok(true) => FencedChildRecycle::Quiescent,
            Ok(false) => FencedChildRecycle::QuiescenceUnproved,
            Err(error) => {
                tracing::error!(target: "plak.topology", agent_id, error = %error,
                    "reopened child quiescence probe failed");
                FencedChildRecycle::QuiescenceUnproved
            }
        }
    };
    let sessions = Arc::clone(&session.sessions);
    let session_id = session.locator().session_id();
    let _ = session.release_opened().await;
    super::result_context::project_feed(&runtime, agent_id, &sessions, session_id)
        .await
        .ok()?;
    Some(recycled)
}

/// The result channel's success means a terminal was delivered, not that the
/// turn succeeded. Handle failures before submission or host intent can mask them.
///
/// The tree projector records a failed terminal's outcome (spec 03 §14.3);
/// this path keeps only the external effects around it. It stops the
/// attempt's jobs, records the failed-worktree snapshot line as the turn's
/// failed-turn notice before the pass joins it into the failure message,
/// then runs the pass, whose seal and result push settle here.
async fn finalize_failed_child_result(
    driver: &TreeDriver,
    identity: &ChildTurnIdentity,
    agent_id: &str,
    feed: Option<&ChildFeedSource>,
    result: &TurnResult,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) -> bool {
    if !matches!(result.outcome(), TurnResultOutcome::Failed(_)) {
        return false;
    }
    let owned = driver
        .read(|tree| identity.still_owns(tree, agent_id))
        .unwrap_or(false);
    // A refused job stop is recorded as the attempt's own outcome, which the
    // pass below then leaves standing.
    if owned {
        let notice = if driver.stop_agent_jobs(&[agent_id.to_owned()]).await.is_ok() {
            failed_worktree::snapshot_failure(driver, identity, agent_id)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };
        mutate_child(driver, agent_id, |tree| {
            if identity.still_owns(tree, agent_id)
                && tree
                    .record_failed_turn_notice(
                        agent_id,
                        &identity.attempt,
                        &identity.turn.to_string(),
                        notice,
                    )
                    .is_ok()
            {
                persist_or_fault(driver, tree, agent_id);
            }
        });
    }
    project_turn_end(driver, feed, agent_id).await;
    mutate_child(driver, agent_id, |tree| {
        if !identity.still_owns(tree, agent_id) {
            return;
        }
        if let Some(detach) = tree
            .node(agent_id)
            .ok()
            .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
        {
            tree.try_emit_tree_node_detached(observer, agent_id, detach);
        }
    });
    true
}

/// How a failed child terminal settles the child's await receipts before
/// any followup can open the next attempt (spec 03 section 4.4).
#[derive(Clone, Copy)]
pub(super) enum FailedTurnWaits<'a> {
    /// It did not: each receipt settles by whether this terminal's committed
    /// history holds its call.
    Settle(&'a TurnResult),
    /// No authoritative terminal: child recovery settles them.
    Keep,
}

pub(super) async fn finalize_child_failure(
    driver: &TreeDriver,
    identity: &ChildTurnIdentity,
    agent_id: &str,
    disposition: crate::agent_tree::turn_outcome::FailedTurnDisposition,
    mut message: String,
    waits: FailedTurnWaits<'_>,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    if !driver
        .read(|tree| identity.still_owns(tree, agent_id))
        .unwrap_or(false)
    {
        return;
    }
    // Recovery-required turns park their service, but must not park running jobs.
    // Keep cleanup failures authoritative rather than overwriting them below.
    if driver
        .stop_agent_jobs(&[agent_id.to_owned()])
        .await
        .is_err()
    {
        return;
    }
    if let Some(snapshot) = failed_worktree::snapshot_failure(driver, identity, agent_id).await {
        message.push_str(&snapshot);
    }
    let pending = driver
        .mutate(|tree| {
            if !identity.still_owns(tree, agent_id) {
                return;
            }
            let evidence = match waits {
                FailedTurnWaits::Settle(terminal) => {
                    super::wait_evidence_in(terminal, tree.retained_wait_receipts(agent_id))
                }
                FailedTurnWaits::Keep => Vec::new(),
            };
            if let Err(error) = tree
                .record_host_turn_failure(agent_id, disposition, message, evidence)
                .and_then(|()| driver.apply_recorded(tree, agent_id))
            {
                tree.record_fault(agent_id, error.message);
            }
        })
        .map(|((), effects)| effects)
        .unwrap_or_default();
    settle_or_fault(driver, pending, agent_id);
    mutate_child(driver, agent_id, |tree| {
        if !identity.still_owns(tree, agent_id) {
            return;
        }
        if let Some(detach) = tree
            .node(agent_id)
            .ok()
            .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
        {
            tree.try_emit_tree_node_detached(observer, agent_id, detach);
        }
    });
}

/// The child session whose feed a turn waiter projects when a turn ends.
pub(super) struct ChildFeedSource {
    sessions: Arc<plakous::sessions::AgentSessions>,
    session: plakous::model::SessionId,
}

impl ChildFeedSource {
    pub(super) fn of(attached: &AttachedCodingSession) -> Self {
        Self {
            sessions: Arc::clone(&attached.sessions),
            session: attached.locator().session_id(),
        }
    }
}

/// Applies the child's feed to its tail when a turn ends (spec 03 §14.3):
/// the terminal's usage folds onto the attempt its command served, as every
/// later pass would derive it. Per-attempt only; cost, quota balances and
/// subtree rollups never reach a model-visible row. A refused read or page
/// is the node's fault; the next pass applies the same records.
async fn project_turn_end(
    driver: &TreeDriver,
    feed: Option<&ChildFeedSource>,
    agent_id: &str,
) -> Option<crate::agent_tree::ChildTerminal> {
    let source = feed?;
    let key = source.session.to_string();
    let cursor = driver.read(|tree| tree.feed_read_cursor(&key)).ok()?;
    let applied = match super::read_tree_feed(&source.sessions, source.session, cursor).await {
        Ok(feed) => super::feed_projection::retry_tree_feed(&source.sessions, feed, |feed| {
            super::feed_projection::apply_on_driver(driver, feed, agent_id)
        })
        .await
        .map(|(projection, _)| projection)
        .map_err(|error| error.message),
        Err(error) => Err(String::from(error)),
    };
    let terminal = mutate_child(driver, agent_id, |tree| match applied {
        Ok(projection) => Ok(projection
            .ended_turns
            .into_iter()
            .filter(|(_, terminal)| {
                tree.attempt(agent_id)
                    .is_some_and(|attempt| attempt.attempt_id == terminal.attempt_id)
            })
            .map(|(_, terminal)| terminal)
            .next_back()),
        Err(error) => {
            tree.record_fault(
                agent_id,
                format!("the child's turn end was not projected: {error}"),
            );
            Err(())
        }
    })?
    .ok()?;
    if terminal.is_some() {
        return terminal;
    }
    // A live tick may already have advanced the cursor past this terminal.
    // Reread only the exact accepted command's bounded evidence, not history.
    match read_finalize_terminal(driver, &source.sessions, source.session, agent_id).await {
        Ok(terminal) => Some(terminal),
        Err(error) => {
            mutate_child(driver, agent_id, |tree| tree.record_fault(agent_id, error));
            None
        }
    }
}

/// Reads only the latest accepted command of this attempt for a late Finalize.
/// The returned terminal belongs to the caller; it is never cached in the tree.
pub(super) async fn read_finalize_terminal(
    driver: &TreeDriver,
    sessions: &plakous::sessions::AgentSessions,
    session_id: plakous::model::SessionId,
    agent_id: &str,
) -> Result<crate::agent_tree::ChildTerminal, String> {
    let (attempt_id, turn, accepted_seq) = driver
        .read(|tree| {
            let attempt = tree.attempt(agent_id)?;
            let command = tree
                .commands()
                .values()
                .filter(|command| {
                    command.recipient == agent_id
                        && command.attempt_id.as_ref() == Some(&attempt.attempt_id)
                        && command.turn_id.is_some()
                        && command.accepted_seq.is_some()
                })
                .max_by_key(|command| command.accepted_seq)?;
            Some((
                attempt.attempt_id.clone(),
                command.turn_id.clone()?,
                command.accepted_seq?,
            ))
        })
        .map_err(|error| error.message)?
        .ok_or_else(|| "child finalization has no accepted command evidence".to_owned())?;
    let feed = super::read_tree_feed(sessions, session_id, accepted_seq).await?;
    driver
        .read(|tree| {
            feed.records
                .iter()
                .flat_map(|record| tree.child_terminal_evidence(agent_id, record))
                .find(|terminal| terminal.turn_id == turn && terminal.attempt_id == attempt_id)
        })
        .map_err(|error| error.message)?
        .ok_or_else(|| "child finalization's exact committed terminal is unavailable".to_owned())
}

/// Whether a child turn ended by cancellation. The runtime reports a joined
/// cancel as `Ok(TurnResult)` with a `Cancelled` outcome — a normal terminal,
/// not an error. Failure to publish terminal authority requires recovery.
pub(super) fn child_turn_was_cancelled(
    terminal: &Result<Arc<TurnResult>, plakous::agent::AgentError>,
) -> bool {
    match terminal {
        Ok(result) => matches!(result.outcome(), TurnResultOutcome::Cancelled(_)),
        Err(_) => false,
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn finalize_topology_child_turn(
    driver: &TreeDriver,
    agent_id: &str,
    cancelled: bool,
    terminal_ok: bool,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    finalize_child_turn(
        driver,
        agent_id,
        None,
        None,
        cancelled,
        terminal_ok,
        observer,
    );
}

fn finalize_child_turn(
    driver: &TreeDriver,
    agent_id: &str,
    identity: Option<&ChildTurnIdentity>,
    terminal: Option<crate::agent_tree::ChildTerminal>,
    cancelled: bool,
    terminal_ok: bool,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) {
    // the seal's snapshot is a VCS subprocess and MUST NOT run
    // under the tree mutex. Phase one marks the node `Sealing` and queues the
    // ticket; phase two runs the snapshot with the lock dropped; phase three
    // re-locks to record the candidate.
    let pending = driver
        .mutate(|tree| {
            if identity.is_some_and(|identity| !identity.still_owns(tree, agent_id)) {
                return;
            }
            if cancelled
                && tree
                    .node(agent_id)
                    .is_ok_and(|node| node.status != crate::agent_tree_protocol::NodeStatus::Closed)
            {
                tree.release_turn_owner(agent_id);
                persist_or_fault(driver, tree, agent_id);
                return;
            }
            if !terminal_ok && !cancelled {
                let recorded = tree.record_host_turn_failure(
                    agent_id,
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                    "child session materialization did not complete".to_owned(),
                    Vec::new(),
                );
                if recorded.is_ok() && persist_or_fault(driver, tree, agent_id) {
                    tree.release_turn_owner(agent_id);
                    if let Err(error) =
                        driver.fold_and_commit(tree, |tree| tree.fold_due(agent_id).map(|_| ()))
                    {
                        tree.record_fault(agent_id, error.message);
                    }
                }
                return;
            }
            let turn = identity
                .map(|identity| identity.turn.to_string())
                .or_else(|| terminal.as_ref().map(|terminal| terminal.turn_id.clone()));
            if let Some(turn) = turn {
                if tree
                    .record_child_finalization(
                        agent_id,
                        &turn,
                        crate::agent_tree::ChildFinalizationDecision::Finalize,
                        true,
                        terminal.clone(),
                    )
                    .is_err()
                    || !persist_or_fault(driver, tree, agent_id)
                {
                    return;
                }
                tree.release_turn_owner(agent_id);
                if let Err(error) =
                    driver.fold_and_commit(tree, |tree| tree.fold_due(agent_id).map(|_| ()))
                {
                    tree.record_fault(agent_id, error.message);
                }
                return;
            }
            // Topology-only fixtures have no runtime feed. Production never
            // substitutes a live observation for committed terminal evidence.
            #[cfg(any(test, feature = "test-support"))]
            if identity.is_none() {
                tree.release_turn_owner(agent_id);
                if terminal_ok {
                    let _ = tree.record_turn_completed(agent_id);
                } else if !cancelled {
                    let _ = tree
                        .record_host_materialization(agent_id)
                        .and_then(|value| driver.apply_recorded(tree, agent_id).map(|()| value));
                }
                if cancelled
                    && tree.node(agent_id).is_ok_and(|node| {
                        node.status == crate::agent_tree_protocol::NodeStatus::Closed
                    })
                {
                    tree.begin_seal(agent_id);
                }
            }
            persist_or_fault(driver, tree, agent_id);
        })
        .map(|((), effects)| effects)
        .unwrap_or_default();
    settle_or_fault(driver, pending, agent_id);
    mutate_child(driver, agent_id, |tree| {
        if identity.is_some_and(|identity| !identity.still_owns(tree, agent_id)) {
            return;
        }
        if cancelled {
            let skip_emit = tree
                .node(agent_id)
                .is_ok_and(|node| !node.status.is_terminal());
            if skip_emit {
                return;
            }
        }
        let status = tree
            .node(agent_id)
            .ok()
            .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
            .unwrap_or(if terminal_ok {
                crate::agent_tree::NodeDetach::Completed
            } else {
                crate::agent_tree::NodeDetach::Failed
            });
        tree.try_emit_tree_node_detached(observer, agent_id, status);
    });
}

fn resume_allowed_in(
    tree: &crate::agent_tree::ProductAgentTree,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> bool {
    identity.still_owns(tree, agent_id)
        && tree
            .node(agent_id)
            .is_ok_and(|node| node.status == crate::agent_tree_protocol::NodeStatus::Running)
        && tree
            .attempt(agent_id)
            .is_some_and(|attempt| !attempt.status.is_stopped() && attempt.output_json.is_none())
        && !tree.attempt_turns_spent(agent_id)
}

fn resume_allowed(driver: &TreeDriver, agent_id: &str, identity: &ChildTurnIdentity) -> bool {
    driver
        .read(|tree| resume_allowed_in(tree, agent_id, identity))
        .unwrap_or(false)
}

async fn wait_transient_gate(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    gate: super::transient::ResumeGate,
) -> bool {
    let probe = live
        .lock()
        .await
        .get(agent_id)
        .and_then(crate::topology_runtime::LiveChildHandle::reachability_probe);
    wait_resume_gate(driver, live.into(), None, agent_id, identity, probe, gate).await
}

pub(super) async fn wait_resume_gate(
    driver: &TreeDriver,
    live: Option<&Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>>,
    startup: Option<&ChildStartup>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    probe: Option<plakous::model::ReachabilityProbe>,
    gate: super::transient::ResumeGate,
) -> bool {
    let cancellation = plakous::model::CancellationToken::new();
    let (probe, delay) = match gate {
        super::transient::ResumeGate::Network => (probe, Duration::from_secs(30)),
        super::transient::ResumeGate::Delay(delay) => (None, delay),
    };
    let ready =
        crate::runtime::network_watch::NetworkWatch::shared().wait(probe, delay, &cancellation);
    tokio::pin!(ready);
    loop {
        let Ok(notify) = driver.read(crate::agent_tree::ProductAgentTree::waiters) else {
            return false;
        };
        let notified = notify.notified_owned();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !resume_allowed(driver, agent_id, identity)
            || startup.is_some_and(ChildStartup::is_cancelled)
            || if let Some(live) = live {
                live.lock()
                    .await
                    .get(agent_id)
                    .is_none_or(|child| child.turn_id() != identity.turn)
            } else {
                false
            }
        {
            cancellation.cancel();
            return false;
        }
        tokio::select! {
            biased;
            () = &mut notified => {}
            value = &mut ready => return value && resume_allowed(driver, agent_id, identity),
            // A hub release may drain the live slot without a tree mutation.
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

async fn try_transient_resume(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    feed: Option<&ChildFeedSource>,
    result: &TurnResult,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) -> Option<Result<TurnResultHandle, ()>> {
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return None;
    };
    let model = error.model_error()?;
    let context = model.context()?;
    let gate = super::transient::classify(model.kind(), context.phase(), context.retryability())?;
    if !resume_allowed(driver, agent_id, identity) {
        return None;
    }
    let round = driver
        .read(|tree| {
            let spent = tree
                .transient_resume_count(agent_id, &identity.attempt)
                .ok()?;
            (spent < tree.snapshot.transient_resumes).then_some(spent + 1)
        })
        .ok()
        .flatten()?;
    let notice = failed_worktree::snapshot_failure(driver, identity, agent_id)
        .await
        .unwrap_or_default();
    let (saved, effects) = driver
        .mutate(|tree| {
            if !identity.still_owns(tree, agent_id) {
                return Err("attempt changed".to_owned());
            }
            tree.record_child_finalization(
                agent_id,
                &identity.turn.to_string(),
                crate::agent_tree::ChildFinalizationDecision::Resume { round },
                false,
                None,
            )
            .map_err(|error| error.message)?;
            // The notice is the projector's join barrier: never release it before
            // the immutable continuation decision is durably saved.
            tree.record_failed_turn_notice(
                agent_id,
                &identity.attempt,
                &identity.turn.to_string(),
                notice,
            )
            .map_err(|error| error.message)?;
            if !persist_or_fault(driver, tree, agent_id) {
                return Err("continuation decision could not be saved".to_owned());
            }
            Ok(())
        })
        .ok()?;
    settle_or_fault(driver, effects, agent_id);
    if saved.is_err() {
        return Some(Err(()));
    }
    project_turn_end(driver, feed, agent_id).await;
    mutate_child(driver, agent_id, |tree| {
        if resume_allowed_in(tree, agent_id, identity) {
            let _ = tree.park_child(agent_id, Vec::new());
            if gate == super::transient::ResumeGate::Network {
                tree.mark_transient_wait(agent_id, true);
            }
        }
    });
    if !wait_transient_gate(driver, live, agent_id, identity, gate).await {
        mutate_child(driver, agent_id, |tree| tree.unpark_child(agent_id));
        return Some(Err(()));
    }
    let command_id = CommandId::new(format!(
        "{}{}:{}:{round}",
        super::result_context::TRANSIENT_RESUME_PREFIX,
        agent_id,
        identity.attempt
    ))
    .ok()?;
    let prompt = crate::child_prompts::TransientResume {
        failure: error.message(),
    }
    .render();
    if let Ok(next) =
        start_instruction_turn(driver, live, agent_id, identity, command_id, prompt).await
    {
        Some(Ok(next))
    } else {
        if resume_allowed(driver, agent_id, identity) {
            let _ = end_attempt_work(driver, live, agent_id, identity).await;
            finalize_child_failure(
                driver,
                identity,
                agent_id,
                crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                    kind: "turn_failed",
                },
                error.message().to_owned(),
                FailedTurnWaits::Settle(result),
                observer,
            )
            .await;
        }
        Some(Err(()))
    }
}

async fn start_submit_harness_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    nudge: SubmitNudgeBudget,
) -> Result<TurnResultHandle, String> {
    let (prompt, command_id) = driver
        .read(|tree| {
            let node = tree.node(agent_id).map_err(|error| error.message)?;
            let attempt = tree
                .attempt(agent_id)
                .ok_or_else(|| "attempt is missing".to_owned())?;
            if attempt.output_json.is_some() || attempt.status.is_stopped() {
                return Err("attempt is already submitted or terminal".into());
            }
            let prompt =
                submit_harness_prompt(nudge.rounds(), attempt.turn_token, &node.output_schema_json);
            let command_id = CommandId::new(format!(
                "plak:submit-harness:{agent_id}:{}:{}:{}",
                tree.host_generation,
                attempt.attempt_id,
                nudge.rounds()
            ))
            .map_err(|error| error.to_string())?;
            Ok::<_, String>((prompt, command_id))
        })
        .map_err(|error| error.message)??;
    let (decision, effects) = driver
        .mutate(|tree| {
            tree.record_child_finalization(
                agent_id,
                &identity.turn.to_string(),
                crate::agent_tree::ChildFinalizationDecision::Nudge,
                false,
                None,
            )
            .map_err(|error| error.message)?;
            if !persist_or_fault(driver, tree, agent_id) {
                return Err("child nudge decision could not be saved".to_owned());
            }
            Ok(())
        })
        .map_err(|error| error.message)?;
    driver.settle(effects).map_err(|error| error.message)?;
    decision?;
    start_instruction_turn(driver, live, agent_id, identity, command_id, prompt).await
}

async fn start_instruction_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    command_id: CommandId,
    prompt: String,
) -> Result<TurnResultHandle, String> {
    let command_text = command_id.as_str().to_owned();
    let result_context = super::result_context::prepare(driver, agent_id, &command_text, &prompt)?;
    let mut live_map = live.lock().await;
    if command_text.starts_with(super::result_context::TRANSIENT_RESUME_PREFIX)
        && !resume_allowed(driver, agent_id, identity)
    {
        super::result_context::acknowledge(
            driver,
            &command_text,
            super::result_context::Submission::Refused,
        )?;
        return Err("transient continuation was cancelled".to_owned());
    }
    let child = live_map
        .get_mut(agent_id)
        .ok_or_else(|| "child live session is missing".to_owned())?;
    if child.turn_id() != identity.turn {
        return Err("child turn has already been replaced".into());
    }
    let result = match child
        .restart(driver.clone(), command_id, result_context.prompt.clone())
        .await
    {
        Ok(result) => result,
        Err(error) => {
            drop(live_map);
            if error == super::live::RESTART_RECONCILIATION_REQUIRED {
                finalize_child_failure(
                    driver,
                    identity,
                    agent_id,
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                    error.clone(),
                    FailedTurnWaits::Keep,
                    None,
                )
                .await;
            }
            return Err(error);
        }
    };
    let turn_id = child.turn_id();
    let cursor = super::live::live_feed_cursor(driver, agent_id)?;
    let read = child.feed(cursor);
    drop(live_map);
    let feed = read.await;
    let (published, effects) = driver
        .mutate(|tree| {
            if !identity.still_owns(tree, agent_id) {
                return Err("child attempt has already been replaced".into());
            }
            if command_text.starts_with(super::result_context::TRANSIENT_RESUME_PREFIX) {
                tree.unpark_child(agent_id);
            }
            tree.bind_turn_owner(agent_id, turn_id.to_string());
            if let Some(feed) = feed? {
                feed.apply(tree, agent_id).map_err(|error| error.message)?;
            }
            // Once Start was admitted the result must retain a waiter even
            // when persistence refuses this publication. The fault remains
            // visible and the acknowledged revision is not advanced.
            persist_or_fault(driver, tree, agent_id);
            Ok::<_, String>(())
        })
        .unwrap_or_else(|error| (Err(error.message), crate::agent_tree::Effects::default()));
    if let Err(error) = published {
        settle_or_fault(driver, effects, agent_id);
        if let Err(retirement_error) =
            crate::topology_runtime::retire_live(driver, live, agent_id, Some(turn_id), true).await
        {
            return Err(format!("{error}; {retirement_error}"));
        }
        return Err(error);
    }
    if let Err(error) = super::result_context::acknowledge(
        driver,
        &command_text,
        super::result_context::Submission::Recorded,
    ) {
        settle_or_fault(driver, effects, agent_id);
        if let Err(retirement_error) =
            crate::topology_runtime::retire_live(driver, live, agent_id, Some(turn_id), true).await
        {
            return Err(format!("{error}; {retirement_error}"));
        }
        return Err(error);
    }
    settle_or_fault(driver, effects, agent_id);
    Ok(result)
}

/// Rejoins the turn a mail Start's reopen recovered unbound: a Start the
/// lost process journaled and the runtime admitted before the tree bound its
/// turn (spec 03 section 14.1). The reopen left that command prepared; it
/// settles from its record before the turn is joined. The turn binds to the
/// attempt before its waiter can observe it, so a turn that ends at once
/// settles as the bound turn and is never marked active again; a turn that
/// loses the live slot is unbound. An attempt that already binds another
/// turn refuses the rejoin (`RecoveryRequired`): the recovered turn is
/// cancelled and released and that binding stays. Only a turn this mail's own Start
/// admitted carried the mail. A turn another command admitted (a lost wake,
/// another mail's Start) is bound and occupied all the same, and the send
/// hears `TurnEnded` and follows up into it, so the mail is never
/// acknowledged by a turn that did not carry it.
pub(super) async fn rejoin_recovered_start(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    message_id: &str,
    attached: AttachedCodingSession,
    turn: TurnId,
    control: plakous::agent::TurnControl,
    result: TurnResultHandle,
) -> Result<crate::agent_tree_tools::MailDeliveryAck, crate::agent_tree_tools::MailDeliveryFailure>
{
    use crate::topology_runtime::LiveChildHandle as _;
    let admitted =
        match super::park::settle_recovered_start(runtime, agent_id, &attached, turn).await {
            Ok(admitted) => admitted,
            Err(error) => {
                let _ = Box::new(crate::agent::live::ProductLiveChild::new(
                    attached, turn, control,
                ))
                .cancel_and_release(runtime.driver().clone())
                .await;
                return Err(error.into());
            }
        };
    // Only the expected state binds: an attempt with no turn binds the
    // recovered one, an attempt that already binds it is left as it is, and
    // an attempt that binds any other turn refuses the rejoin, which cancels
    // and releases the recovered turn below without touching that binding.
    // The turn's waiter registers with the bind and is dropped only after a
    // lost occupy unbound it, so the bound turn never reads as unwatched.
    let mut registration = None;
    let bound = runtime.prepare_then_emit(|tree| {
        let current = tree
            .attempt(agent_id)
            .ok_or_else(|| "recovered child turn has no attempt to bind".to_owned())?
            .turn_id
            .clone();
        match current {
            None => {
                registration = TurnWaiterRegistration::enter(runtime.driver(), tree, agent_id);
                Ok(true)
            }
            Some(current) if current == turn.to_string() => {
                registration = TurnWaiterRegistration::enter(runtime.driver(), tree, agent_id);
                Ok(false)
            }
            Some(current) => Err(format!(
                "recovered child turn {turn} does not match the turn {current} the attempt binds"
            )),
        }
    });
    match bound {
        Ok(_) => {}
        Err(error) => {
            let _ = Box::new(crate::agent::live::ProductLiveChild::new(
                attached, turn, control,
            ))
            .cancel_and_release(runtime.driver().clone())
            .await;
            return Err(error.into());
        }
    }
    let occupied = occupy_recovered_turn(
        runtime,
        agent_id.to_owned(),
        attached,
        turn,
        control,
        result,
        &mut registration,
    )
    .await;
    drop(registration);
    let occupied = occupied?;
    #[cfg(test)]
    super::park::hooks::after_recovered_occupy(agent_id).await;
    if admitted == mail_command_id(message_id, "start")?.as_str() && occupied == turn {
        return Ok(crate::agent_tree_tools::MailDeliveryAck {
            turn_id: Some(turn.to_string()),
        });
    }
    Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded)
}

pub(super) async fn occupy_recovered_child(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: String,
    attached: AttachedCodingSession,
    recovered: RecoveredTurnHandle,
) -> Result<TurnId, String> {
    let (turn, control, result) = (recovered.turn_id(), recovered.control(), recovered.result());
    let mut registration = TurnWaiterRegistration::register(runtime.driver(), &agent_id);
    occupy_recovered_turn(
        runtime,
        agent_id,
        attached,
        turn,
        control,
        result,
        &mut registration,
    )
    .await
}

/// Occupies the live slot with a recovered turn and hands it, with the
/// waiter `registration` taken before the slot became visible, to a turn
/// waiter. A slot another turn won answers that turn and leaves the
/// registration with the caller.
async fn occupy_recovered_turn(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: String,
    attached: AttachedCodingSession,
    turn: TurnId,
    control: plakous::agent::TurnControl,
    result: TurnResultHandle,
    registration: &mut Option<TurnWaiterRegistration>,
) -> Result<TurnId, String> {
    let feed = ChildFeedSource::of(&attached);
    let child: Box<dyn crate::topology_runtime::LiveChildHandle> = Box::new(
        crate::agent::live::ProductLiveChild::new(attached, turn, control),
    );
    match runtime.try_occupy_live(agent_id.clone(), child).await {
        Ok(turn_id) => {
            if let Some(registration) = registration.take() {
                spawn_topology_turn_waiter(
                    registration,
                    Arc::clone(&runtime.live),
                    runtime.observer(),
                    Some(feed),
                    result,
                );
            }
            Ok(turn_id)
        }
        Err(loser) => {
            let occupied = runtime
                .live
                .lock()
                .await
                .get(&agent_id)
                .map(crate::topology_runtime::LiveChildHandle::turn_id);
            if let Some(failure) = loser.release(runtime.driver().clone()).await.failure {
                return Err(failure);
            }
            match occupied {
                Some(existing) => Ok(existing),
                None => Err("topology hub is shutting down".into()),
            }
        }
    }
}

pub(super) struct InflightChild {
    agent_id: String,
    locator: String,
    path: String,
    turn_id: String,
}

pub(super) fn inflight_children(tree: &crate::agent_tree::ProductAgentTree) -> Vec<InflightChild> {
    tree.inflight_join_targets()
        .into_iter()
        .filter(|(agent_id, ..)| {
            tree.pending_transient_resume(agent_id)
                .ok()
                .flatten()
                .is_none()
        })
        .map(|(agent_id, locator, path, turn_id)| InflightChild {
            agent_id,
            locator,
            path,
            turn_id,
        })
        .collect()
}

pub(super) async fn reattach_inflight_children(
    parent: &SpawnParent,
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
) {
    let Ok(paused) =
        runtime.read(|tree| tree.state == crate::agent_tree::TreeAdmissionState::RestoredPaused)
    else {
        return;
    };
    if paused {
        // RestoredPaused: do not occupy recovered child turns until ResumeTree.
        return;
    }
    let _reattach = runtime.lock_reattach().await;
    let Ok(paused) =
        runtime.read(|tree| tree.state == crate::agent_tree::TreeAdmissionState::RestoredPaused)
    else {
        return;
    };
    if paused {
        return;
    }
    let Ok(children) = runtime.read(inflight_children) else {
        return;
    };
    let mut failed = Vec::new();
    let mut joined = Vec::new();
    let mut abandoned_children = Vec::new();
    let mut failure_reasons: Vec<(String, String)> = Vec::new();
    for child in children {
        if runtime.live.lock().await.contains_key(&child.agent_id)
            || runtime.is_recycling(&child.agent_id)
        {
            joined.push(child.agent_id);
            continue;
        }
        let _stored_turn = if let Ok(turn_id) = TurnId::from_str(&child.turn_id) {
            turn_id
        } else {
            failed.push(child.agent_id);
            continue;
        };
        match reopen_spawned_child(
            parent,
            &child.agent_id,
            &child.path,
            &child.locator,
            "reattach",
            ChildReopenMode::OWNER,
        )
        .await
        {
            Ok(ChildReopen::Attached {
                session: attached,
                recovered: Some(recovered),
            }) if bound_child_turn_matches(runtime, &child.agent_id, recovered.turn_id()) => {
                match occupy_recovered_child(runtime, child.agent_id.clone(), *attached, recovered)
                    .await
                {
                    Ok(_) => joined.push(child.agent_id),
                    Err(_) => failed.push(child.agent_id),
                }
            }
            Ok(ChildReopen::Attached {
                session: attached, ..
            }) => {
                // Process death dropped the live turn. The lifecycle contract
                // forbids a replacement Start (restart drain). A reopen that
                // abandoned the turn the lost process left settles it as an
                // ordinary failure the parent can re-task; a quiescent head
                // with nothing abandoned while the tree still lists the child
                // inflight stays RecoveryRequired, not a silent Completed.
                let abandoned = attached
                    .agent()
                    .and_then(plakous::agent::Agent::abandoned_turn)
                    .cloned();
                // Read before the release, at the head the abandonment left;
                // an unreadable history restores every receipt's mail.
                let retained = runtime
                    .read(|tree| tree.retained_wait_receipts(&child.agent_id))
                    .unwrap_or_default();
                let evidence = match super::committed_wait_evidence(
                    attached.sessions(),
                    attached.locator(),
                    retained.clone(),
                )
                .await
                {
                    Ok(evidence) => evidence,
                    Err(_) => retained.into_iter().map(|id| (id, false)).collect(),
                };
                let _ = attached.release_opened().await;
                match abandoned {
                    Some(report) => abandoned_children.push((child.agent_id, report, evidence)),
                    None => failed.push(child.agent_id),
                }
            }
            Ok(ChildReopen::RecoveryRequired(report)) => {
                failure_reasons.push((
                    child.agent_id.clone(),
                    format!(
                        "the child's session is recovery-required after the process was lost \
                         ({:?})",
                        report.phase()
                    ),
                ));
                match recover_reattach_effect_evidence(parent, &child.agent_id, &report).await {
                    Ok(RecoverEffectEvidenceOutcome::Finalized { target, .. }) => {
                        let locator = target.to_string();
                        let projected = match parent.state_roots().runtime_sessions() {
                            Ok(sessions) => {
                                super::result_context::project_feed(
                                    runtime,
                                    &child.agent_id,
                                    &sessions,
                                    target.session_id(),
                                )
                                .await
                            }
                            Err(error) => Err(error.to_string()),
                        };
                        if let Err(error) = projected {
                            failure_reasons.push((child.agent_id.clone(), error));
                            failed.push(child.agent_id);
                            continue;
                        }
                        match reopen_spawned_child(
                            parent,
                            &child.agent_id,
                            &child.path,
                            &locator,
                            "reattach-finalized",
                            ChildReopenMode::OWNER,
                        )
                        .await
                        {
                            Ok(ChildReopen::Attached {
                                session: attached,
                                recovered: Some(recovered),
                            }) if bound_child_turn_matches(
                                runtime,
                                &child.agent_id,
                                recovered.turn_id(),
                            ) =>
                            {
                                match occupy_recovered_child(
                                    runtime,
                                    child.agent_id.clone(),
                                    *attached,
                                    recovered,
                                )
                                .await
                                {
                                    Ok(_) => joined.push(child.agent_id),
                                    Err(_) => failed.push(child.agent_id),
                                }
                            }
                            Ok(ChildReopen::Attached {
                                session: attached, ..
                            }) => {
                                let _ = attached.release_opened().await;
                                failed.push(child.agent_id);
                            }
                            _ => failed.push(child.agent_id),
                        }
                    }
                    _ => failed.push(child.agent_id),
                }
            }
            Err(reason) => {
                failure_reasons.push((child.agent_id.clone(), reason.message));
                failed.push(child.agent_id);
            }
        }
    }
    // Spec 03 §14.1: a child the lost process left parked wakes once, with
    // whatever its stopped processes and its mailbox now hold.
    let owner = runtime
        .read(|tree| tree.tree_id.clone())
        .ok()
        .and_then(|tree_id| crate::topology_runtime::lookup(&tree_id));
    let parked = owner
        .as_ref()
        .and_then(|owner| {
            owner
                .read(crate::agent_tree::ProductAgentTree::parked_reopen_targets)
                .ok()
        })
        .unwrap_or_default();
    for (agent_id, locator, path) in parked {
        // The resume held this child for its reopen wake; whatever happens
        // to that wake below, the hold ends with this iteration.
        let held = agent_id.clone();
        let release_hold = || {
            mutate_child(runtime.driver(), &held, |tree| {
                tree.release_reopen_hold(&held);
            });
        };
        let Some(owner) = owner.as_ref() else {
            release_hold();
            continue;
        };
        if runtime.live.lock().await.contains_key(&agent_id) || runtime.is_recycling(&agent_id) {
            release_hold();
            continue;
        }
        match reopen_spawned_child(
            parent,
            &agent_id,
            &path,
            &locator,
            "park-wake",
            ChildReopenMode::REJOIN,
        )
        .await
        {
            Ok(ChildReopen::Attached {
                session,
                recovered: None,
            }) => {
                if let Some(report) = session
                    .agent()
                    .and_then(plakous::agent::Agent::abandoned_turn)
                    .cloned()
                {
                    // Same evidence read as the in-flight reattach above.
                    let retained = runtime
                        .read(|tree| tree.retained_wait_receipts(&agent_id))
                        .unwrap_or_default();
                    let evidence = match super::committed_wait_evidence(
                        session.sessions(),
                        session.locator(),
                        retained.clone(),
                    )
                    .await
                    {
                        Ok(evidence) => evidence,
                        Err(_) => retained.into_iter().map(|id| (id, false)).collect(),
                    };
                    let _ = session.release_opened().await;
                    abandoned_children.push((agent_id, report, evidence));
                    release_hold();
                    continue;
                }
                // An interrupt that ended the hold leaves the child
                // `interrupted`: nothing to wake and nothing to recover.
                if super::park::wake_reopened(owner, &agent_id, *session)
                    .await
                    .is_err()
                {
                    failed.push(agent_id);
                }
            }
            // A wake or mail Start the lost process journaled was admitted
            // before its turn bound: that turn rejoins (spec 03 §14.1). The
            // rejoin binds the turn active itself, and never over an
            // interrupt that ended the hold, so it is not in `joined`.
            Ok(ChildReopen::Attached {
                session,
                recovered: Some(recovered),
            }) => {
                if super::park::rejoin_reopened(owner, &agent_id, *session, recovered)
                    .await
                    .is_err()
                {
                    failed.push(agent_id);
                }
            }
            Ok(ChildReopen::RecoveryRequired(_)) => failed.push(agent_id),
            Err(reason) => {
                failure_reasons.push((agent_id.clone(), reason.message));
                failed.push(agent_id);
            }
        }
        release_hold();
    }
    let live_ids: std::collections::HashSet<String> =
        runtime.live.lock().await.keys().cloned().collect();
    mutate_child(
        runtime.driver(),
        &runtime
            .read(|tree| tree.root_agent_id.clone())
            .unwrap_or_default(),
        |tree| {
            let observer = parent.state_roots().topology_observer();
            for (agent_id, report, evidence) in &abandoned_children {
                if live_ids.contains(agent_id) {
                    continue;
                }
                let recorded = tree.record_host_turn_failure(
                    agent_id,
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                        kind: crate::agent_tree::TURN_ABANDONED,
                    },
                    crate::runtime::events::abandoned_turn_operator_line(report).to_owned(),
                    evidence.clone(),
                );
                if recorded.is_err() || runtime.driver().apply_recorded(tree, agent_id).is_err() {
                    continue;
                }
                if let Some(status) = tree
                    .node(agent_id)
                    .ok()
                    .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
                {
                    tree.try_emit_tree_node_detached(observer.as_ref(), agent_id, status);
                }
            }
            for (agent_id, reason) in &failure_reasons {
                if failed.contains(agent_id) && !live_ids.contains(agent_id) {
                    tree.record_fault(agent_id, reason.clone());
                }
            }
            for agent_id in &failed {
                if live_ids.contains(agent_id) {
                    continue;
                }
                let terminal = tree
                    .node(agent_id)
                    .is_ok_and(|node| node.status.is_terminal())
                    || tree
                        .attempt(agent_id)
                        .is_some_and(|assignment| assignment.status.is_stopped());
                if terminal {
                    continue;
                }
                let _ = tree
                    .record_host_materialization(agent_id)
                    .and_then(|value| {
                        runtime
                            .driver()
                            .apply_recorded(tree, agent_id)
                            .map(|()| value)
                    });
                let status = tree
                    .node(agent_id)
                    .ok()
                    .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
                    .unwrap_or(crate::agent_tree::NodeDetach::RecoveryRequired);
                tree.try_emit_tree_node_detached(observer.as_ref(), agent_id, status);
            }
            let root_id = tree.root_agent_id.clone();
            persist_or_fault(runtime.driver(), tree, &root_id);
        },
    );
}

/// How long a child reopen waits for a lease its lost parent still held: one
/// lease period plus slack, since the dead holder renews for at most that long.
pub(super) const CHILD_LEASE_WAIT: Duration = Duration::from_secs(35);
pub(super) const CHILD_LEASE_POLL: Duration = Duration::from_secs(2);

pub(super) enum ChildReopen {
    Attached {
        session: Box<AttachedCodingSession>,
        recovered: Option<RecoveredTurnHandle>,
    },
    RecoveryRequired(RecoveryRequiredReport),
}

/// How one child reopen behaves around the parts a caller may not want.
#[derive(Clone, Copy, Debug)]
pub(super) struct ChildReopenMode {
    /// How long to wait out a lease another owner still holds before
    /// refusing with [`ChildReopenErrorKind::LeaseHeld`].
    pub(super) lease_wait: Duration,
    /// A recovery reopen records nothing on the node but a moved session
    /// locator: no abandoned-turn fact (its settlement carries that), and no
    /// retirement failure when the result reconciliation refuses.
    pub(super) recovery: bool,
    /// The caller rejoins a recovered turn its attempt does not bind (a
    /// Start the lost process journaled and the runtime admitted before the
    /// bind), so the result reconcile leaves that turn's command prepared
    /// for the rejoin to settle. Every other caller settles it here.
    pub(super) rejoin: bool,
}

impl ChildReopenMode {
    /// A reopen by the process that owns the child: waits out a lease the
    /// lost owner still held and records what it finds on the node.
    pub(super) const OWNER: Self = Self {
        lease_wait: CHILD_LEASE_WAIT,
        recovery: false,
        rejoin: false,
    };

    /// An owner reopen that rejoins an unbound recovered turn itself.
    pub(super) const REJOIN: Self = Self {
        rejoin: true,
        ..Self::OWNER
    };
}

/// Why a child reopen refused, typed so callers map it without reading text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChildReopenErrorKind {
    /// Another owner still holds the child session's lease.
    LeaseHeld {
        /// When the store said the lease expires, if it did.
        retry_after_seconds: Option<u64>,
    },
    /// The session store is saturated or unavailable.
    StorageUnavailable,
    /// The session was written by, or reconstructs differently under,
    /// another plak build.
    Incompatible,
    /// The tree, the binding, or the session could not be reopened for any
    /// other reason.
    SessionUnavailable,
}

impl ChildReopenErrorKind {
    /// Whether a refused create of this kind, after which the exact
    /// accepted-create lookup finds no session, proves the spawn never
    /// produced a session. Storage and a held lease do not: both are
    /// transient, and the create stays retryable work.
    pub(super) const fn settles_never_created(&self) -> bool {
        matches!(self, Self::SessionUnavailable | Self::Incompatible)
    }

    fn of(error: &AgentSessionsError) -> Self {
        match error.kind() {
            AgentSessionsErrorKind::Ownership => Self::LeaseHeld {
                retry_after_seconds: None,
            },
            AgentSessionsErrorKind::StorageFull
            | AgentSessionsErrorKind::StorageBusy
            | AgentSessionsErrorKind::StorageUnavailable
            | AgentSessionsErrorKind::StoragePressure => Self::StorageUnavailable,
            AgentSessionsErrorKind::IncompatibleBuild => Self::Incompatible,
            _ => Self::SessionUnavailable,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ChildReopenError {
    pub(super) kind: ChildReopenErrorKind,
    pub(super) message: String,
}

impl ChildReopenError {
    pub(super) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ChildReopenErrorKind::SessionUnavailable,
            message: message.into(),
        }
    }

    pub(super) fn from_sessions(prefix: &str, error: &AgentSessionsError) -> Self {
        Self {
            kind: ChildReopenErrorKind::of(error),
            message: format_session_reopen_error(prefix, error),
        }
    }
}

impl std::fmt::Display for ChildReopenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<ChildReopenError> for String {
    fn from(error: ChildReopenError) -> Self {
        error.message
    }
}

impl From<ChildReopenError> for crate::agent_tree_tools::MailDeliveryFailure {
    fn from(error: ChildReopenError) -> Self {
        Self::RecoveryRequired(error.message)
    }
}

pub(super) const SESSION_LEASE_HOLD_HINT: &str =
    "session lease is still held; retry after 30s. This is not RecoveryRequired.";

pub(super) fn format_session_reopen_error(prefix: &str, error: &AgentSessionsError) -> String {
    match error.kind() {
        AgentSessionsErrorKind::IncompatibleBuild => incompatible_session_message(error),
        AgentSessionsErrorKind::Ownership => {
            format!("{prefix} (Ownership): {SESSION_LEASE_HOLD_HINT} {error}")
        }
        kind => format!("{prefix} ({kind:?}): {error}"),
    }
}

/// The one refusal every surface prints for a session another build wrote.
pub(crate) fn incompatible_session_message(error: &AgentSessionsError) -> String {
    error
        .to_string()
        .replace("an incompatible build", "an incompatible plak build")
}

/// Product does not persist executor outcomes outside Plakous. SIGKILL during
/// a Working mutate therefore has no caller-owned evidence to supply.
pub(super) fn product_owned_child_effect_evidence(
    report: &RecoveryRequiredReport,
) -> Option<(ToolExecutorId, SessionEffectEvidence, ToolOutcome)> {
    let _ = report;
    None
}

pub(super) async fn recover_reattach_effect_evidence(
    parent: &SpawnParent,
    agent_id: &str,
    report: &RecoveryRequiredReport,
) -> Result<RecoverEffectEvidenceOutcome, String> {
    let Some(obligation) =
        report.obligations().iter().copied().find(|obligation| {
            obligation.action() == SessionRecoveryAction::EffectEvidenceRequired
        })
    else {
        return Err("child reopen has no effect-evidence obligation".into());
    };
    let Some((executor, evidence, outcome)) = product_owned_child_effect_evidence(report) else {
        return Err("no executor-owned effect evidence; RecoveryRequired".into());
    };
    let generation = crate::topology_runtime::lookup(spawn_parent_tree_id(parent))
        .and_then(|runtime| runtime.read(|tree| tree.host_generation).ok())
        .unwrap_or(1);
    let operation_id =
        SessionOperationId::new(format!("plak:recover-effect:{agent_id}:{generation}"))
            .map_err(|_| "child recover-effect operation identity is invalid".to_owned())?;
    let sessions = parent
        .state_roots()
        .runtime_sessions()
        .map_err(|_| "plak runtime service is unavailable".to_owned())?;
    sessions
        .recover_effect_evidence(RecoverEffectEvidenceRequest::new(
            operation_id,
            report.locator(),
            obligation.operation_id(),
            evidence,
            executor,
            outcome,
        ))
        .await
        .map_err(|error| error.to_string())
}

pub(super) async fn reopen_spawned_child(
    parent: &SpawnParent,
    agent_id: &str,
    path: &str,
    locator: &str,
    operation_kind: &str,
    mode: ChildReopenMode,
) -> Result<ChildReopen, ChildReopenError> {
    Box::pin(reopen_spawned_child_at_head(
        parent,
        agent_id,
        path,
        locator,
        operation_kind,
        mode,
    ))
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "one reopen: stale-head adoption, lease wait, and the outcome's reconciliation"
)]
async fn reopen_spawned_child_at_head(
    parent: &SpawnParent,
    agent_id: &str,
    path: &str,
    locator: &str,
    operation_kind: &str,
    mode: ChildReopenMode,
) -> Result<ChildReopen, ChildReopenError> {
    let tree_id = spawn_parent_tree_id(parent).to_owned();
    let topology = crate::topology_runtime::lookup(&tree_id).ok_or_else(|| {
        ChildReopenError::unavailable("topology hub is not attached; cannot reopen child")
    })?;
    let bind = topology
        .read(|tree| {
            tree.node(agent_id)
                .map(ChildBoundBind::of)
                .map_err(|error| error.message)
        })
        .map_err(|error| error.message)
        .and_then(|bind| bind)
        .map_err(ChildReopenError::unavailable)?;
    let (account, tuple_id, effort) = (bind.account, bind.tuple_id, bind.effort);
    let fence = topology
        .read(|tree| {
            tree.node(agent_id)
                .ok()
                .map(|node| occupancy_session_fence(node.tier))
        })
        .ok()
        .flatten()
        .unwrap_or(SessionToolFence::OccupancyReadOnly);
    let sessions = parent
        .state_roots()
        .runtime_sessions()
        .map_err(|_| ChildReopenError::unavailable("plak runtime service is unavailable"))?;
    let mut locator = SessionLocator::from_str(locator).map_err(|error| {
        ChildReopenError::unavailable(format!("child session locator is invalid: {error}"))
    })?;
    let generation = topology.read(|tree| tree.host_generation).unwrap_or(1);
    // The tree's locator can lag the branch head (a turn that ended
    // without the node hearing about it). Open the head
    // the store names instead and record it, once; a second stale answer is a
    // real fault.
    let mut opened = None;
    // A lease the lost parent still held expires on its own clock; waiting
    // it out is how the reopen gets to abandon the turn that parent left.
    let lease_wait_until = tokio::time::Instant::now() + mode.lease_wait;
    let mut lease_waits = 0_u32;
    let mut attempt = 0_u32;
    while attempt < 2 {
        // Read before the child's configuration is compiled, and re-read on the
        // second attempt because adoption moved the locator. A reopened child
        // keeps the output bound it was created with for the same reason a
        // reopened root does: the figure reaches the configuration identity and
        // from there the child's persisted plan, projection target, request
        // prefix and admission evidence, none of which can be re-signed.
        let created_output_bound =
            match Box::pin(sessions.created_request_output_bound(locator)).await {
                Ok(bound) => bound,
                // The head moved past the recorded locator, typically because
                // the lost turn advanced it. Adopt it exactly as a stale open does.
                Err(error) if let Some(current) = child_reopen_head(&error, attempt != 0) => {
                    attempt += 1;
                    locator = current;
                    continue;
                }
                Err(error) => {
                    return Err(ChildReopenError::from_sessions(
                        "child created output bound is unavailable",
                        &error,
                    ));
                }
            };
        let roots = parent
            .state_roots()
            .clone()
            .for_exact_reopen(&sessions, locator)
            .await
            .map_err(|error| {
                ChildReopenError::from_sessions("child recorded evaluator is unavailable", &error)
            })?;
        let mut scoped_parent = parent.clone();
        match &mut scoped_parent {
            SpawnParent::Native(spec) => spec.state_roots = roots,
            SpawnParent::Api(spec) => spec.state_roots = roots,
        }
        let runtime = child_session_runtime_binding(
            &scoped_parent,
            agent_id,
            path,
            account.as_deref(),
            tuple_id.as_deref(),
            effort.as_deref(),
            true,
            fence,
            Some(created_output_bound),
        )
        .await
        .map_err(ChildReopenError::unavailable)?;
        let operation_id = SessionOperationId::new(format!(
            "plak:{operation_kind}:{agent_id}:{generation}:{attempt}:{lease_waits}"
        ))
        .map_err(|_| ChildReopenError::unavailable("child reopen operation identity is invalid"))?;
        match Box::pin(sessions.open(OpenSessionRequest::new(operation_id, locator, runtime))).await
        {
            Ok(OpenSessionOutcome::RecoveryRequired {
                retry_after_seconds: Some(retry_after),
                ..
            }) => {
                let now = tokio::time::Instant::now();
                if now >= lease_wait_until {
                    return Err(ChildReopenError {
                        kind: ChildReopenErrorKind::LeaseHeld {
                            retry_after_seconds: Some(retry_after),
                        },
                        message: format!(
                            "child session lease is still held by another owner; retry after \
                             {retry_after}s"
                        ),
                    });
                }
                lease_waits = lease_waits.saturating_add(1);
                tokio::time::sleep(CHILD_LEASE_POLL.min(lease_wait_until - now)).await;
            }
            Ok(outcome) => {
                opened = Some(outcome);
                break;
            }
            Err(error) if let Some(current) = child_reopen_head(&error, attempt != 0) => {
                attempt += 1;
                locator = current;
            }
            Err(error) => {
                return Err(ChildReopenError::from_sessions(
                    "child session could not be reopened",
                    &error,
                ));
            }
        }
    }
    let Some(opened) = opened else {
        return Err(ChildReopenError::unavailable(
            "child session could not be reopened at its current head",
        ));
    };
    let incompatible = |error: super::CodingSessionOpenFailure| ChildReopenError {
        kind: ChildReopenErrorKind::Incompatible,
        message: error.failure.message().to_owned(),
    };
    let reopened = match opened {
        OpenSessionOutcome::Opened { session, .. } => {
            require_reconstructed_coding_projection(&session).map_err(incompatible)?;
            let locator = session.locator();
            ChildReopen::Attached {
                session: Box::new(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: None,
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: None,
                    locator,
                    topology_owner: None,
                }),
                recovered: None,
            }
        }
        OpenSessionOutcome::RestoredWorking { session, turn, .. } => {
            require_reconstructed_coding_projection(&session).map_err(incompatible)?;
            let locator = session.locator();
            ChildReopen::Attached {
                session: Box::new(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: None,
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: Some(turn.clone()),
                    locator,
                    topology_owner: None,
                }),
                recovered: Some(turn),
            }
        }
        OpenSessionOutcome::RecoveryRequired { recovery, .. } => {
            ChildReopen::RecoveryRequired(recovery)
        }
        _ => {
            return Err(ChildReopenError::unavailable(
                "child session reopen was unavailable",
            ));
        }
    };
    reconcile_reopened_child_results(&topology, agent_id, reopened, mode)
        .await
        .map_err(ChildReopenError::unavailable)
}

fn bound_child_turn_matches(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    turn: TurnId,
) -> bool {
    runtime
        .read(|tree| {
            tree.attempt(agent_id)
                .and_then(|attempt| attempt.turn_id.as_deref())
                == Some(turn.to_string().as_str())
        })
        .unwrap_or(false)
}

/// Settles a reopened child's command intents from its feed's
/// `CommandAccepted` and `CommandRejected` facts through the tree projector.
/// `recovered` is the turn the caller rejoins, if any. A rejoined turn on an
/// attempt that binds none is a Start the lost process wrote and the runtime
/// admitted before the tree bound it: the rejoin (or the mail Start that
/// reopened the child) applies the feed itself, under its reopen hold, and
/// finds that command by its fact (spec 03 section 14.1).
pub(super) async fn reconcile_reopened_results(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    session: &AttachedCodingSession,
    recovered: Option<TurnId>,
) -> Result<(), String> {
    let expected = runtime
        .read(|tree| {
            tree.attempt(agent_id)
                .and_then(|attempt| attempt.turn_id.clone())
        })
        .map_err(|error| error.message)?
        .map(|turn| {
            turn.parse::<TurnId>()
                .map_err(|_| "bound child turn is invalid".to_owned())
        })
        .transpose()?;
    if recovered.is_some() && expected.is_none() {
        return Ok(());
    }
    // Only unresolved command intents need their facts here; with none, the
    // reopen's own feed pass (recovery's release, the wake's preparation)
    // applies the feed with the terminal reads it needs.
    if super::result_context::prepared_commands(runtime.driver(), agent_id)?.is_empty() {
        return Ok(());
    }
    super::result_context::project_child_feed(runtime, agent_id, session)
        .await
        .map(|_| ())
}

async fn reconcile_reopened_child_results(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    reopened: ChildReopen,
    mode: ChildReopenMode,
) -> Result<ChildReopen, String> {
    use crate::topology_runtime::LiveChildHandle as _;
    let ChildReopen::Attached { session, recovered } = reopened else {
        return Ok(reopened);
    };
    // Only a caller that rejoins the recovered turn keeps its command
    // prepared; a recovery or recycle reopen settles everything from the
    // records.
    let rejoining = recovered
        .as_ref()
        .filter(|_| mode.rejoin)
        .map(RecoveredTurnHandle::turn_id);
    let reconciled = async {
        if !mode.recovery {
            let cursor = runtime
                .read(|tree| tree.feed_read_cursor(&session.locator().session_id().to_string()))
                .map_err(|error| error.message)?;
            let feed =
                super::read_tree_feed(&session.sessions, session.locator().session_id(), cursor)
                    .await?;
            super::feed_projection::retry_tree_feed(&session.sessions, feed, |feed| {
                super::feed_projection::apply_on_runtime(runtime, feed, agent_id)
            })
            .await
            .map_err(|error| error.message)?;
        }
        reconcile_reopened_results(runtime, agent_id, &session, rejoining).await
    }
    .await;
    if let Err(error) = reconciled {
        let sessions = Arc::clone(&session.sessions);
        let session_id = session.locator().session_id();
        let release = if let Some(recovered) = recovered {
            Box::new(crate::agent::live::ProductLiveChild::new(
                *session,
                recovered.turn_id(),
                recovered.control(),
            ))
            .cancel_and_release(runtime.driver().clone())
            .await
        } else {
            match session.release_opened().await {
                Ok(locator) => LiveRelease {
                    feed: None,
                    locator: Some(locator.to_string()),
                    failure: None,
                },
                Err(release) => LiveRelease {
                    feed: None,
                    locator: None,
                    failure: Some(release.to_string()),
                },
            }
        };
        let error = release
            .failure
            .map_or(error.clone(), |release| format!("{error}; {release}"));
        if mode.recovery {
            return Err(error);
        }
        super::result_context::project_feed(runtime, agent_id, &sessions, session_id).await?;
        runtime.record_then_apply(agent_id, |tree| {
            tree.record_host_retirement(agent_id, error.clone())
        })?;
        return Err(error);
    }
    Ok(ChildReopen::Attached { session, recovered })
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use crate::topology_runtime::{LiveChildHandle, LiveRelease, ProductTopologyRuntime};
    use plakous::agent::TurnControl;

    struct CleanupChild {
        turn: TurnId,
        failure: bool,
    }

    impl LiveChildHandle for CleanupChild {
        fn agent(&self) -> Option<plakous::agent::Agent> {
            None
        }
        fn turn_id(&self) -> TurnId {
            self.turn
        }
        fn turn_control(&self) -> TurnControl {
            panic!("unused fixture control")
        }
        fn follow_up(
            &self,
            _: CommandId,
            _: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
        fn restart(
            &mut self,
            _: TreeDriver,
            _: CommandId,
            _: String,
        ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
            Box::pin(async { Err("unused".into()) })
        }
        fn release(
            self: Box<Self>,
            driver: TreeDriver,
        ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
            self.cancel_and_release(driver)
        }
        fn cancel_and_release(
            self: Box<Self>,
            _driver: TreeDriver,
        ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
            Box::pin(async move {
                LiveRelease {
                    feed: None,
                    locator: None,
                    failure: self.failure.then(|| "fixture retirement failed".into()),
                }
            })
        }
    }

    #[tokio::test]
    async fn attempt_end_retires_self_and_all_descendants_after_one_retirement_fails() {
        use crate::agent_tree_protocol::SpawnAgentRequest;
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let (parent, children, identity) = runtime
            .mutate_then_emit(false, |tree| {
                let mut snapshot = tree.snapshot.clone();
                snapshot.max_depth = 2;
                snapshot.digest = snapshot.compute_digest();
                tree.install_snapshot(snapshot);
                let parent = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                    .unwrap()
                    .agent_id;
                tree.bind_attempt_turn(&parent, turn.to_string()).unwrap();
                let children = (0..2)
                    .map(|_| {
                        tree.spawn_agent(&parent, &SpawnAgentRequest::test_review(), None)
                            .unwrap()
                            .agent_id
                    })
                    .collect::<Vec<_>>();
                let identity = ChildTurnIdentity {
                    attempt: tree.attempt(&parent).unwrap().attempt_id.clone(),
                    turn,
                };
                Ok((parent, children, identity))
            })
            .unwrap();
        for (id, failure) in [
            (parent.clone(), false),
            (children[0].clone(), true),
            (children[1].clone(), false),
        ] {
            assert!(
                runtime
                    .try_occupy_live(id, Box::new(CleanupChild { turn, failure }))
                    .await
                    .is_ok()
            );
        }
        assert!(
            end_attempt_work(runtime.driver(), &runtime.live, &parent, &identity)
                .await
                .is_err()
        );
        let live = runtime.live.lock().await;
        assert!(
            !live.contains_key(&parent),
            "own live owner must retire despite descendant failure"
        );
        assert!(
            !live.contains_key(&children[1]),
            "other descendants must still retire"
        );
    }

    fn runtime(root: &Path) -> Arc<ProductTopologyRuntime> {
        Arc::new(ProductTopologyRuntime::new(
            crate::topology_runtime::test_support::bootstrap(
                "child-retirement",
                root.to_path_buf(),
            ),
            crate::topology_runtime::test_support::memory_store(),
            Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
            None,
        ))
    }

    struct CloseSaveStore {
        inner: crate::agent_tree::MemoryTreeStore,
        saves: std::sync::atomic::AtomicUsize,
        fail_at: std::sync::atomic::AtomicUsize,
        stale: bool,
        prepared: std::sync::Mutex<Option<crate::agent_tree::PersistedTree>>,
    }

    #[test]
    fn refused_waiter_bind_does_not_unregister_a_concurrent_waiter() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for stale in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = Arc::new(CloseSaveStore {
                inner: crate::agent_tree::MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(0),
                stale,
                prepared: std::sync::Mutex::new(None),
            });
            let runtime = ProductTopologyRuntime::new(
                crate::topology_runtime::test_support::bootstrap(
                    "waiter-bind-refusal",
                    root.path().to_path_buf(),
                ),
                store.clone(),
                Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
                None,
            );
            let child = runtime
                .mutate_then_emit(false, |tree| {
                    Ok(tree
                        .spawn_agent(
                            "root",
                            &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                            None,
                        )
                        .unwrap()
                        .agent_id)
                })
                .unwrap();
            let concurrent = TurnWaiterRegistration::register(runtime.driver(), &child).unwrap();
            store
                .fail_at
                .store(store.saves.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
            let mut registration = None;
            let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
            assert!(
                register_admitted_turn(
                    &runtime,
                    StartAdmission::Ordinary,
                    &child,
                    turn,
                    &mut registration
                )
                .is_err()
            );
            drop(registration);
            assert!(runtime.read(|tree| tree.has_turn_waiter(&child)).unwrap());
            drop(concurrent);
            assert!(!runtime.read(|tree| tree.has_turn_waiter(&child)).unwrap());
        }
    }

    impl crate::agent_tree::TreeStore for CloseSaveStore {
        fn exists(&self, id: &str) -> Result<bool, crate::agent_tree::TreeStoreError> {
            crate::agent_tree::TreeStore::exists(&self.inner, id)
        }
        fn ids(&self) -> Result<Vec<String>, crate::agent_tree::TreeStoreError> {
            crate::agent_tree::TreeStore::ids(&self.inner)
        }
        fn load(
            &self,
            id: &str,
        ) -> Result<Option<crate::agent_tree::PersistedTree>, crate::agent_tree::TreeStoreError>
        {
            crate::agent_tree::TreeStore::load(&self.inner, id)
        }
        fn save(
            &self,
            tree: &crate::agent_tree::ProductAgentTree,
        ) -> Result<crate::agent_tree::Revision, crate::agent_tree::TreeStoreError> {
            use std::sync::atomic::Ordering;
            let count = self.saves.fetch_add(1, Ordering::SeqCst) + 1;
            if count == self.fail_at.load(Ordering::SeqCst) {
                return Err(if self.stale {
                    crate::agent_tree::TreeStoreError::stale(
                        tree.stored_revision + 1,
                        tree.revision,
                    )
                } else {
                    crate::agent_tree::TreeStoreError::unavailable("close consequence refused")
                });
            }
            let revision = crate::agent_tree::TreeStore::save(&self.inner, tree)?;
            if tree
                .operations()
                .values()
                .any(|op| op.tool_name == "seal_effect" && op.terminal_response.is_none())
            {
                *self.prepared.lock().unwrap() =
                    crate::agent_tree::TreeStore::load(&self.inner, &tree.tree_id)?;
            }
            Ok(revision)
        }
    }

    #[tokio::test]
    async fn hosted_descendant_close_refusal_and_commit_cut_never_dispatch_unpublished_seals() {
        use crate::agent_tree::{ProductAgentTree, TreeStore as _};
        use crate::agent_tree_protocol::{SpawnAgentRequest, Tier, WriterMode};
        use crate::occupancy_vcs::fake::{FakeOp, FakeVcs};
        use std::sync::atomic::{AtomicUsize, Ordering};
        for spent in [false, true] {
            for stale in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let vcs = Arc::new(FakeVcs::new());
                let store = Arc::new(CloseSaveStore {
                    inner: crate::agent_tree::MemoryTreeStore::new(),
                    saves: AtomicUsize::new(0),
                    fail_at: AtomicUsize::new(0),
                    stale,
                    prepared: std::sync::Mutex::new(None),
                });
                let tree = ProductAgentTree::bootstrap_root_for_test_with(
                    vcs.clone(),
                    "hosted-close-cut",
                    "root",
                    "owner",
                    root.path().to_path_buf(),
                    "selected-tuple",
                    WriterMode::SingleLease,
                )
                .unwrap();
                let runtime = ProductTopologyRuntime::new(tree, store.clone(), vcs.clone(), None);
                let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
                let (parent, child, identity) = runtime
                    .mutate_then_emit(false, |tree| {
                        let mut snapshot = tree.snapshot.clone();
                        snapshot.max_depth = 2;
                        snapshot.digest = snapshot.compute_digest();
                        tree.install_snapshot(snapshot);
                        let parent = tree
                            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                            .unwrap()
                            .agent_id;
                        tree.bind_attempt_turn(&parent, turn.to_string()).unwrap();
                        let child = tree
                            .spawn_agent(
                                &parent,
                                &SpawnAgentRequest::test_named(
                                    "implement",
                                    Tier::Mutate,
                                    "worker",
                                    "task",
                                )
                                .with_fork(),
                                None,
                            )
                            .unwrap()
                            .agent_id;
                        let identity = ChildTurnIdentity {
                            attempt: tree.attempt(&parent).unwrap().attempt_id.clone(),
                            turn,
                        };
                        Ok((parent, child, identity))
                    })
                    .unwrap();
                let before = runtime
                    .read(|tree| {
                        (
                            tree.node(&child).unwrap().clone(),
                            tree.attempt(&child).unwrap().clone(),
                        )
                    })
                    .unwrap();
                store
                    .fail_at
                    .store(store.saves.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                let terminal = crate::agent_tree::ChildTerminal {
                    attempt_id: identity.attempt.clone(),
                    turn_id: turn.to_string(),
                    committed_at_ms: 1000,
                    outcome: plakous::sessions::feed::FeedTurnOutcome::Completed {
                        finish: plakous::model::FinishReason::Stop,
                    },
                    seq: 1,
                };
                let refused = if spent {
                    end_spent_attempt(&runtime, &parent, terminal.clone()).await
                } else {
                    end_attempt_work(runtime.driver(), &runtime.live, &parent, &identity).await
                };
                assert!(refused.is_err());
                assert!(vcs.calls_of(FakeOp::Seal).is_empty());
                runtime
                    .read(|tree| {
                        assert_eq!(tree.node(&child).unwrap(), &before.0);
                        assert_eq!(tree.attempt(&child).unwrap(), &before.1);
                    })
                    .unwrap();
                // A successful retry publishes the reservation before dispatch. Capture that
                // save as the crash cut between closure and the effect-result publication.
                // end_attempt_work records the parent's retirement on refusal;
                // the released/spent recovery path retries descendant closure.
                end_spent_attempt(&runtime, &parent, terminal)
                    .await
                    .unwrap();
                assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
                let persisted = store
                    .prepared
                    .lock()
                    .unwrap()
                    .take()
                    .expect("seal reservation published before dispatch");
                let mut reopened = ProductAgentTree::reopen(persisted);
                reopened.fold_due(&child).unwrap();
                assert!(reopened.take_effects().seals.is_empty());
                reopened.close_live_descendants(&parent).unwrap();
                assert!(reopened.take_effects().seals.is_empty());
                assert!(store.load("hosted-close-cut").unwrap().is_some());
            }
        }
    }

    #[test]
    fn startup_caller_drop_without_runtime_does_not_panic() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let startup = executor
            .block_on(runtime.reserve_startup("root"))
            .unwrap()
            .unwrap();
        let caller = StartupCaller::new(runtime.clone(), "root".into(), startup.clone());
        assert!(tokio::runtime::Handle::try_current().is_err());
        drop(caller);
        assert!(startup.is_cancelled());
        executor
            .block_on(runtime.finish_startup("root", &startup, StartupWorker::failure()))
            .unwrap();
    }

    #[test]
    fn startup_worker_drop_without_runtime_does_not_panic() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let startup = executor
            .block_on(runtime.reserve_startup("root"))
            .unwrap()
            .unwrap();
        let worker = StartupWorker::new(runtime.clone(), "root".into(), startup.clone());
        assert!(tokio::runtime::Handle::try_current().is_err());
        drop(worker);
        executor
            .block_on(runtime.finish_startup("root", &startup, StartupWorker::failure()))
            .unwrap();
    }

    #[tokio::test]
    async fn late_terminal_cannot_mutate_a_replacement_turn() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let old = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let new = TurnId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let (child, identity) = runtime
            .mutate_then_emit(false, |tree| {
                let child = tree
                    .spawn_agent(
                        "root",
                        &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                        None,
                    )
                    .unwrap()
                    .agent_id;
                tree.bind_attempt_turn(&child, old.to_string()).unwrap();
                let identity = ChildTurnIdentity {
                    attempt: tree.attempt(&child).unwrap().attempt_id.clone(),
                    turn: old,
                };
                tree.bind_attempt_turn(&child, new.to_string()).unwrap();
                Ok((child, identity))
            })
            .unwrap();
        let before = runtime.read(|tree| tree.revision).unwrap();
        finalize_child_turn(
            runtime.driver(),
            &child,
            Some(&identity),
            None,
            false,
            false,
            None,
        );
        finalize_child_failure(
            runtime.driver(),
            &identity,
            &child,
            crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
            "late failure".into(),
            FailedTurnWaits::Keep,
            None,
        )
        .await;
        runtime
            .read(|tree| {
                assert_eq!(tree.revision, before);
                assert_eq!(
                    tree.attempt(&child).unwrap().turn_id.as_deref(),
                    Some(new.to_string().as_str())
                );
                assert_eq!(
                    tree.node(&child).unwrap().status,
                    crate::agent_tree_protocol::NodeStatus::Running
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn failed_child_snapshot_error_keeps_original_failure_and_partial_candidate() {
        use crate::agent_tree::turn_outcome::FailedTurnDisposition;
        use crate::agent_tree_protocol::{SpawnAgentRequest, Tier};
        use crate::occupancy_vcs::{VcsError, fake::FakeVcs};
        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let runtime = ProductTopologyRuntime::new(
            crate::topology_runtime::test_support::bootstrap(
                "failed-snapshot",
                root.path().to_path_buf(),
            ),
            crate::topology_runtime::test_support::memory_store(),
            vcs.clone(),
            None,
        );
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let (child, identity) = runtime
            .mutate_then_emit(false, |tree| {
                let child = tree
                    .spawn_agent(
                        "root",
                        &SpawnAgentRequest::test_named(
                            "implement",
                            Tier::Mutate,
                            "implement",
                            "work",
                        )
                        .with_fork(),
                        None,
                    )
                    .unwrap()
                    .agent_id;
                tree.bind_attempt_turn(&child, turn.to_string()).unwrap();
                let identity = ChildTurnIdentity {
                    attempt: tree.attempt(&child).unwrap().attempt_id.clone(),
                    turn,
                };
                Ok((child, identity))
            })
            .unwrap();
        vcs.seal_next_with(Err(VcsError::seal_failed("injected snapshot failure")));
        finalize_child_failure(
            runtime.driver(),
            &identity,
            &child,
            FailedTurnDisposition::Failed {
                kind: "turn_failed",
            },
            "original provider failure".into(),
            FailedTurnWaits::Keep,
            None,
        )
        .await;
        runtime
            .read(|tree| {
                let attempt = tree.attempt(&child).unwrap();
                let failure = attempt.failure.as_ref().unwrap();
                assert_eq!(failure.kind, "turn_failed");
                assert!(failure.message.contains("original provider failure"));
                assert!(
                    failure.message.contains(
                        "failed worktree snapshot unavailable: injected snapshot failure"
                    )
                );
                assert!(attempt.candidate_id.is_some());
            })
            .unwrap();
    }

    #[tokio::test]
    async fn failed_child_snapshot_is_reported_for_forks_only() {
        use crate::agent_tree::turn_outcome::FailedTurnDisposition;
        use crate::agent_tree_protocol::{SpawnAgentRequest, Tier};
        for fork in [false, true] {
            for recovery in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let runtime = runtime(root.path());
                let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
                let (child, identity) = runtime
                    .mutate_then_emit(false, |tree| {
                        let mut request = SpawnAgentRequest::test_named(
                            "implement",
                            Tier::Mutate,
                            "implement",
                            "work",
                        );
                        if fork {
                            request = request.with_fork();
                        }
                        let child = tree.spawn_agent("root", &request, None).unwrap().agent_id;
                        tree.bind_attempt_turn(&child, turn.to_string()).unwrap();
                        let identity = ChildTurnIdentity {
                            attempt: tree.attempt(&child).unwrap().attempt_id.clone(),
                            turn,
                        };
                        Ok((child, identity))
                    })
                    .unwrap();
                finalize_child_failure(
                    runtime.driver(),
                    &identity,
                    &child,
                    if recovery {
                        FailedTurnDisposition::RecoveryRequired
                    } else {
                        FailedTurnDisposition::Failed {
                            kind: "turn_failed",
                        }
                    },
                    "provider failure".into(),
                    FailedTurnWaits::Keep,
                    None,
                )
                .await;
                runtime
                    .read(|tree| {
                        let attempt = tree.attempt(&child).unwrap();
                        let message = &attempt.failure.as_ref().unwrap().message;
                        assert!(message.contains("provider failure"));
                        assert_eq!(
                            message.contains("failed worktree snapshot: refs/plak/"),
                            fork
                        );
                        assert_eq!(attempt.candidate_id.is_some(), fork && !recovery);
                    })
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn submitted_child_uses_authoritative_failed_terminal_instead_of_channel_success() {
        use crate::agent_tree_protocol::{
            AttemptScopedRequest, AttemptStatus, NodeStatus, SpawnAgentRequest,
            SubmitResultRequest, Tier,
        };
        use plakous::model::ModelErrorKind;
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
            SessionOperationId,
        };
        for (kind, publication_fails) in [
            (ModelErrorKind::Protocol, false),
            (ModelErrorKind::OutcomeUnknown, false),
            (ModelErrorKind::QuotaExceeded, false),
            (ModelErrorKind::RateLimited, false),
            (ModelErrorKind::ContextLimit, false),
            (ModelErrorKind::Policy, false),
            (ModelErrorKind::Protocol, true),
        ] {
            for close_first in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let runtime = runtime(root.path());
                let evidence = root.path().join("evidence.jsonl");
                let release = root.path().join("release");
                // A fast failing model may retire the active turn before
                // start() returns. Hold dispatch until its control is captured.
                let controller = plakous_offline_crash_harness::BoundaryController::quiet_gated_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::ProviderDispatchOne,
                    &evidence, &release,
                ).unwrap();
                let binding = plakous_offline_crash_harness::terminal_failure_runtime_binding(
                    root.path(),
                    controller,
                    kind,
                )
                .unwrap();
                let database = root.path().join("runtime.sqlite3");
                let backend = SessionBackend::sqlite(&database).unwrap();
                let sessions = Arc::new(AgentSessions::new(backend).unwrap());
                let created = sessions
                    .create(CreateSessionRequest::new(
                        SessionOperationId::new("terminal-fixture:create").unwrap(),
                        binding,
                    ))
                    .await
                    .unwrap();
                assert!(matches!(&created, CreateSessionOutcome::Created { .. }));
                let CreateSessionOutcome::Created { session, .. } = created else {
                    continue;
                };
                let submission = session
                    .agent()
                    .start(
                        CommandId::new("terminal-fixture:start").unwrap(),
                        "fail terminal",
                    )
                    .await
                    .unwrap();
                let handle = submission.result().unwrap().clone();
                let turn = handle.turn_id();
                let control = session.agent().turn_control(turn).unwrap();
                tokio::time::timeout(Duration::from_secs(20), async {
                    while !plakous_offline_crash_harness::evidence::read_evidence(&evidence)
                        .is_ok_and(|rows| rows.iter().any(|row| row.boundary == plakous_offline_crash_harness::protocol::CrashBoundary::ProviderDispatchOne))
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                if publication_fails {
                    rusqlite::Connection::open(&database).unwrap().execute_batch(
                        "CREATE TRIGGER refuse_terminal BEFORE INSERT ON checkpoints BEGIN SELECT RAISE(FAIL, 'injected terminal publication failure'); END;"
                    ).unwrap();
                }
                std::fs::write(&release, "release").unwrap();
                let terminal = handle.clone().result().await;
                let recovery = kind == ModelErrorKind::OutcomeUnknown || publication_fails;
                if publication_fails {
                    assert!(matches!(
                        terminal.as_ref().unwrap().outcome(),
                        TurnResultOutcome::RecoveryRequired(_)
                    ));
                } else {
                    let terminal = terminal.unwrap();
                    assert_eq!(
                        matches!(terminal.outcome(), TurnResultOutcome::RecoveryRequired(_)),
                        recovery
                    );
                    if !recovery {
                        assert!(matches!(terminal.outcome(), TurnResultOutcome::Failed(_)));
                    }
                }
                let child = runtime
                    .mutate_then_emit(false, |tree| {
                        let child = tree
                            .spawn_agent(
                                "root",
                                &SpawnAgentRequest::test_named(
                                    "implement",
                                    Tier::Mutate,
                                    "implement",
                                    "work",
                                )
                                .with_fork(),
                                None,
                            )
                            .unwrap()
                            .agent_id;
                        tree.bind_attempt_turn(&child, turn.to_string()).unwrap();
                        tree.set_active_turn(&child, true).unwrap();
                        let token = tree.attempt(&child).unwrap().turn_token;
                        tree.submit_result(
                            &child,
                            &SubmitResultRequest {
                                turn_token: token,
                                output_json: r#"{"ok":true}"#.into(),
                            },
                            None,
                        )
                        .unwrap();
                        if close_first {
                            tree.close_agent(
                                "root",
                                &AttemptScopedRequest {
                                    agent_id: child.clone(),
                                    expected_attempt_id: None,
                                },
                                None,
                            )
                            .unwrap();
                        }
                        Ok(child)
                    })
                    .unwrap();
                let locator = session.locator();
                let registry = Arc::new(crate::jobs::JobRegistry::default());
                let environment = plakous::tools::ExecutionEnvironment::host(
                    plakous::tools::ExecutionEnvironmentScope::Run(
                        plakous::model::RunId::from_str("00000000-0000-0000-0000-000000000706")
                            .unwrap(),
                    ),
                    root.path(),
                    ".",
                    plakous::tools::ExecutionEnvironmentConfig::default(),
                )
                .unwrap();
                let job_host = registry.for_agent(child.clone(), "s");
                let service = job_host
                    .attach(
                        plakous::tools::JobOwner {
                            session_id: locator.session_id(),
                            branch_id: locator.branch_id(),
                            agent_id: plakous::model::AgentId::from_str(
                                "00000000-0000-0000-0000-000000000707",
                            )
                            .unwrap(),
                        },
                        environment.joined_process_sessions().unwrap(),
                    )
                    .unwrap();
                let job = service
                    .start(
                        plakous::tools::JoinedProcessStartRequest::new(
                            plakous::tools::JoinedProcessCommand::shell(
                                plakous::tools::JoinedProcessShellProfile::new(
                                    "/bin/sh",
                                    ["-c".into()],
                                )
                                .unwrap(),
                                "exec sleep 3600",
                            )
                            .unwrap(),
                            ".",
                            Vec::new(),
                            false,
                            plakous::tools::JoinedProcessTerminalMode::Pipes,
                        )
                        .unwrap(),
                        plakous::tools::ExecutionOperationControl::new(
                            plakous::model::CancellationToken::new(),
                        ),
                    )
                    .unwrap();
                plakous_tools::internal::promote(&service, job).unwrap();
                runtime
                    .mutate_then_emit(false, |tree| {
                        tree.bind_session_locator(&child, locator.to_string())
                            .unwrap();
                        Ok(())
                    })
                    .unwrap();
                crate::agent_tree_tools::invoke_granted_topology_tool_on(
                    runtime.driver().clone(),
                    "root".into(),
                    "list_agents",
                    &serde_json::json!({}),
                    "bind-test-jobs".into(),
                    crate::agent_tree_tools::TopologyHostPorts {
                        job_host: Some(registry.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                assert_eq!(registry.snapshot()[0].owner, child);
                let feed = ChildFeedSource {
                    sessions: Arc::clone(&sessions),
                    session: locator.session_id(),
                };
                let live_child = super::super::live::ProductLiveChild::new(
                    AttachedCodingSession {
                        output_bound_rederived: None,
                        reachability_probe: None,
                        sessions,
                        opened: Some(session),
                        recovered_turn: None,
                        locator,
                        topology_owner: None,
                    },
                    turn,
                    control,
                );
                assert!(
                    runtime
                        .try_occupy_live(child.clone(), Box::new(live_child))
                        .await
                        .is_ok()
                );
                spawn_topology_turn_waiter(
                    TurnWaiterRegistration::register(runtime.driver(), &child).unwrap(),
                    runtime.live.clone(),
                    None,
                    Some(feed),
                    handle,
                );
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !runtime.live.lock().await.is_empty()
                        || runtime
                            .read(|tree| {
                                let attempt = tree.attempt(&child).unwrap();
                                !(matches!(
                                    attempt.status,
                                    AttemptStatus::Failed
                                        | AttemptStatus::Blocked
                                        | AttemptStatus::RecoveryRequired
                                ) || (attempt.status == AttemptStatus::Closed
                                    && attempt.failure.is_some()))
                            })
                            .unwrap()
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap_or_else(|_| {
                    let state = runtime.read(|tree| {
                        format!(
                            "{:?} {:?} {:?}",
                            tree.attempt(&child).map(|attempt| attempt.status),
                            tree.node(&child).map(|node| node.status),
                            tree.fault_reason(&child)
                        )
                    });
                    panic!("{kind:?} close_first={close_first}: the attempt never ended: {state:?}")
                });
                assert_eq!(
                    service.jobs()[0].status,
                    plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::AttemptEnded),
                    "failed and recovery-required child jobs must not remain parked",
                );
                assert_eq!(registry.snapshot()[0].owner, child);
                assert_eq!(
                    registry.snapshot()[0].status,
                    plakous_host_protocol::views::ProcessStatus::Stopped
                );
                runtime
                    .read(|tree| {
                        let attempt = tree.attempt(&child).unwrap();
                        assert_eq!(attempt.output_json.as_deref(), Some(r#"{"ok":true}"#));
                        assert_eq!(attempt.usage.turns, 1, "no submit harness turn");
                        assert_eq!(
                            tree.node(&child).unwrap().status,
                            if recovery {
                                NodeStatus::RecoveryRequired
                            } else if close_first {
                                NodeStatus::Closed
                            } else {
                                NodeStatus::Failed
                            }
                        );
                        assert_eq!(
                            attempt.status,
                            if close_first && recovery {
                                AttemptStatus::Closed
                            } else if recovery {
                                AttemptStatus::RecoveryRequired
                            } else if kind == ModelErrorKind::QuotaExceeded {
                                AttemptStatus::Blocked
                            } else {
                                AttemptStatus::Failed
                            }
                        );
                        assert_eq!(attempt.candidate_id.is_some(), !recovery);
                        if kind != ModelErrorKind::QuotaExceeded {
                            assert!(
                                attempt
                                    .failure
                                    .as_ref()
                                    .unwrap()
                                    .message
                                    .contains("failed worktree snapshot: refs/plak/")
                            );
                        }
                        if kind == ModelErrorKind::RateLimited {
                            assert_eq!(attempt.failure.as_ref().unwrap().kind, "rate_limited");
                        }
                        if kind == ModelErrorKind::ContextLimit && !recovery {
                            let failure = attempt.failure.as_ref().unwrap();
                            assert_eq!(failure.kind, "context_exhausted");
                            assert!(failure.message.contains("respawn with a handoff"));
                            assert!(failure.message.contains("not a follow-up"));
                        }
                        if kind == ModelErrorKind::Policy && !recovery {
                            let failure = attempt.failure.as_ref().unwrap();
                            assert_eq!(failure.kind, "refused");
                            assert!(failure.message.contains("respawn with a handoff"));
                            assert!(failure.message.contains("not a follow-up"));
                            assert!(failure.message.contains("injected terminal model failure"));
                        }
                        if recovery {
                            assert_eq!(
                                attempt.failure.as_ref().unwrap().kind,
                                "turn_recovery_required"
                            );
                        }
                    })
                    .unwrap();
            }
        }
    }

    struct EndingChild {
        turn: TurnId,
        entered: Arc<tokio::sync::Notify>,
        finish: Arc<tokio::sync::Notify>,
        failure: bool,
    }
    impl LiveChildHandle for EndingChild {
        fn agent(&self) -> Option<plakous::agent::Agent> {
            None
        }
        fn turn_id(&self) -> TurnId {
            self.turn
        }
        fn turn_control(&self) -> TurnControl {
            panic!("fixture does not need control")
        }
        fn follow_up(
            &self,
            _: CommandId,
            _: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
            Box::pin(async { Err("fixture".into()) })
        }
        fn restart(
            &mut self,
            _: TreeDriver,
            _: CommandId,
            _: String,
        ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
            Box::pin(async {
                Err(
                    "duplicate child restart has no live turn; result reconciliation required"
                        .into(),
                )
            })
        }
        fn release(
            self: Box<Self>,
            _driver: TreeDriver,
        ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.finish.notified().await;
                LiveRelease {
                    feed: None,
                    locator: (!self.failure).then(|| "completed-head".into()),
                    failure: self.failure.then(|| "release refused".into()),
                }
            })
        }
        fn cancel_and_release(
            self: Box<Self>,
            _driver: TreeDriver,
        ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
            panic!("terminal retirement must win cancellation")
        }
    }

    #[tokio::test]
    async fn retired_duplicate_restart_marks_child_recovery_required() {
        use crate::agent_tree_protocol::{AttemptStatus, NodeStatus, SpawnAgentRequest};
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let child = runtime
            .mutate_then_emit(false, |tree| {
                let child = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .unwrap();
                tree.bind_attempt_turn(&child.agent_id, turn.to_string())
                    .unwrap();
                Ok(child)
            })
            .unwrap();
        runtime
            .try_occupy_live(
                child.agent_id.clone(),
                Box::new(EndingChild {
                    turn,
                    entered: Arc::new(tokio::sync::Notify::new()),
                    finish: Arc::new(tokio::sync::Notify::new()),
                    failure: false,
                }),
            )
            .await
            .map_err(|_| "live slot refused")
            .unwrap();
        let identity = ChildTurnIdentity {
            attempt: child.attempt_id,
            turn,
        };
        let error = start_submit_harness_turn(
            runtime.driver(),
            &runtime.live,
            &child.agent_id,
            &identity,
            SubmitNudgeBudget::NONE.next().unwrap(),
        )
        .await
        .err()
        .unwrap();
        assert!(error.contains("result reconciliation required"));
        runtime
            .read(|tree| {
                assert_eq!(
                    tree.node(&child.agent_id).unwrap().status,
                    NodeStatus::RecoveryRequired
                );
                assert_eq!(
                    tree.attempt(&child.agent_id).unwrap().status,
                    AttemptStatus::RecoveryRequired
                );
                assert!(tree.commands().values().any(|command| {
                    command.recipient == child.agent_id && command.unresolved()
                }));
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn transient_gate_interrupt_wins_without_waiting_for_delay() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let child = runtime
            .mutate_then_emit(false, |tree| {
                let child = tree
                    .spawn_agent(
                        "root",
                        &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                        None,
                    )
                    .unwrap();
                tree.bind_attempt_turn(&child.agent_id, turn.to_string())
                    .unwrap();
                Ok(child)
            })
            .unwrap();
        runtime
            .try_occupy_live(
                child.agent_id.clone(),
                Box::new(EndingChild {
                    turn,
                    entered: Arc::new(tokio::sync::Notify::new()),
                    finish: Arc::new(tokio::sync::Notify::new()),
                    failure: false,
                }),
            )
            .await
            .map_err(|_| "live slot refused")
            .unwrap();
        let identity = ChildTurnIdentity {
            attempt: child.attempt_id,
            turn,
        };
        let gate = wait_transient_gate(
            runtime.driver(),
            &runtime.live,
            &child.agent_id,
            &identity,
            crate::agent::transient::ResumeGate::Delay(Duration::from_secs(60)),
        );
        tokio::pin!(gate);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(gate.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        runtime
            .mutate_then_emit(false, |tree| {
                tree.interrupt_agent(
                    "root",
                    &crate::agent_tree_protocol::AttemptScopedRequest {
                        agent_id: child.agent_id.clone(),
                        expected_attempt_id: None,
                    },
                    None,
                )
                .map(|_| ())
                .map_err(|error| error.message)
            })
            .unwrap();
        assert!(!gate.await);
    }

    #[tokio::test]
    async fn submit_harness_receives_results_and_supports_followup_attempts() {
        use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest};
        use plakous::sessions::{AgentSessions, SessionBackend};
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, runtime_binding};

        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let parent = runtime
            .mutate_then_emit(false, |tree| {
                let mut snapshot = tree.snapshot.clone();
                snapshot.max_depth = 2;
                snapshot.digest = snapshot.compute_digest();
                tree.install_snapshot(snapshot);
                let parent = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                    .unwrap();
                let child = tree
                    .spawn_agent(&parent.agent_id, &SpawnAgentRequest::test_review(), None)
                    .unwrap();
                tree.submit_result(
                    &child.agent_id,
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: r#"{"ok":true}"#.to_owned(),
                    },
                    None,
                )
                .unwrap();
                tree.record_turn_completed(&child.agent_id).unwrap();
                Ok(parent)
            })
            .unwrap();
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.path().join("harness-result.jsonl"),
        )
        .unwrap();
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("harness-result.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("harness-result-create").unwrap(),
                runtime_binding(root.path(), controller).unwrap(),
            ))
            .await
            .unwrap()
        else {
            panic!("new session required")
        };
        let submission = session
            .agent()
            .start(CommandId::new("harness-result-first").unwrap(), "first")
            .await
            .unwrap();
        let result = submission.result().unwrap().clone();
        let turn_id = result.turn_id();
        let control = session.agent().turn_control(turn_id).unwrap();
        result.result().await.unwrap();
        let locator = session.locator();
        runtime
            .mutate_then_emit(false, |tree| {
                tree.bind_attempt_turn(&parent.agent_id, turn_id.to_string())
                    .map_err(|error| error.message)
            })
            .unwrap();
        runtime
            .try_occupy_live(
                parent.agent_id.clone(),
                Box::new(crate::agent::live::ProductLiveChild::new(
                    AttachedCodingSession {
                        output_bound_rederived: None,
                        reachability_probe: None,
                        sessions,
                        opened: Some(session),
                        recovered_turn: None,
                        locator,
                        topology_owner: None,
                    },
                    turn_id,
                    control,
                )),
            )
            .await
            .map_err(|_| "live slot refused")
            .unwrap();
        let identity = ChildTurnIdentity {
            attempt: parent.attempt_id.clone(),
            turn: turn_id,
        };
        let result = start_submit_harness_turn(
            runtime.driver(),
            &runtime.live,
            &parent.agent_id,
            &identity,
            SubmitNudgeBudget::NONE.next().unwrap(),
        )
        .await
        .unwrap();
        let mut completed_turn = result.turn_id();
        result.result().await.unwrap();
        assert!(
            runtime
                .read(|tree| tree.pending_result_mail(&parent.agent_id).is_empty())
                .unwrap()
        );
        assert!(
            runtime
                .read(|tree| tree.commands().values().any(|command| {
                    command.kind == crate::agent_tree::AgentCommandKind::Corrective
                        && command.text.contains("child_result")
                        && command.fact == Some(crate::agent_tree::CommandFact::Accepted)
                }))
                .unwrap()
        );
        // The same node and host generation must admit both corrective rounds
        // again after followup rotates the assignment and its submission token.
        for assignment in 0..2 {
            let first_round = if assignment == 0 { 2 } else { 1 };
            for round in first_round..=2 {
                let identity = ChildTurnIdentity {
                    attempt: runtime
                        .read(|tree| tree.attempt(&parent.agent_id).unwrap().attempt_id.clone())
                        .unwrap(),
                    turn: completed_turn,
                };
                let mut nudge = SubmitNudgeBudget::NONE;
                for _ in 0..round {
                    nudge = nudge.next().unwrap();
                }
                let result = start_submit_harness_turn(
                    runtime.driver(),
                    &runtime.live,
                    &parent.agent_id,
                    &identity,
                    nudge,
                )
                .await
                .expect("each assignment owns its corrective Start identities");
                completed_turn = result.turn_id();
                result.result().await.unwrap();
                let (command, prompt, operations_before) = runtime
                    .read(|tree| {
                        let suffix = format!(":{round}");
                        let command = tree
                            .commands()
                            .values()
                            .find(|command| {
                                command.recipient == parent.agent_id
                                    && command.kind
                                        == crate::agent_tree::AgentCommandKind::Corrective
                                    && command.attempt_id.as_ref() == Some(&identity.attempt)
                                    && command.command_id.ends_with(&suffix)
                            })
                            .unwrap();
                        (
                            command.command_id.clone(),
                            submit_harness_prompt(
                                round,
                                tree.attempt(&parent.agent_id).unwrap().turn_token,
                                &tree.node(&parent.agent_id).unwrap().output_schema_json,
                            ),
                            tree.commands().len(),
                        )
                    })
                    .unwrap();
                super::super::result_context::prepare(
                    runtime.driver(),
                    &parent.agent_id,
                    &command,
                    &prompt,
                )
                .expect("same-attempt corrective context replays exactly");
                super::super::result_context::acknowledge(
                    runtime.driver(),
                    &command,
                    super::super::result_context::Submission::Recorded,
                )
                .unwrap();
                assert_eq!(
                    runtime.read(|tree| tree.commands().len()).unwrap(),
                    operations_before
                );
            }
            runtime
                .mutate_then_emit(false, |tree| {
                    assert_eq!(
                        tree.attempt(&parent.agent_id)
                            .unwrap()
                            .submit_harness_nudge
                            .rounds(),
                        2
                    );
                    tree.record_turn_completed(&parent.agent_id).unwrap();
                    Ok(())
                })
                .unwrap();
            if assignment == 0 {
                runtime
                    .mutate_then_emit(false, |tree| {
                        tree.send_agent_message(
                            "root",
                            &crate::agent_tree_protocol::SendAgentMessageRequest {
                                agent_id: parent.agent_id.clone(),
                                message: "followup assignment".into(),
                                purpose: Some(crate::agent_tree_protocol::MessagePurpose::Followup),
                                priority: None,
                                in_reply_to: None,
                                output_schema_json: None,
                            },
                            None,
                        )
                        .unwrap();
                        assert_ne!(
                            tree.attempt(&parent.agent_id).unwrap().attempt_id,
                            parent.attempt_id
                        );
                        Ok(())
                    })
                    .unwrap();
                let result = runtime
                    .live
                    .lock()
                    .await
                    .get_mut(&parent.agent_id)
                    .unwrap()
                    .restart(
                        runtime.driver().clone(),
                        CommandId::new("harness-followup-start").unwrap(),
                        "followup assignment".into(),
                    )
                    .await
                    .unwrap();
                completed_turn = result.turn_id();
                result.result().await.unwrap();
                runtime
                    .mutate_then_emit(false, |tree| {
                        tree.bind_attempt_turn(&parent.agent_id, completed_turn.to_string())
                            .unwrap();
                        Ok(())
                    })
                    .unwrap();
            }
        }
        crate::topology_runtime::retire_live(
            runtime.driver(),
            &runtime.live,
            &parent.agent_id,
            None,
            false,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reopened_child_reconciles_durable_result_command_without_redispatch() {
        assert_reopened_result_reconciliation(false, false).await;
    }

    #[tokio::test]
    async fn reopened_corrective_child_reconciles_prior_bound_turn_without_redispatch() {
        assert_reopened_result_reconciliation(true, false).await;
    }

    #[tokio::test]
    async fn reopened_corrective_child_without_results_preserves_round_budget() {
        assert_reopened_result_reconciliation(true, true).await;
    }

    async fn assert_reopened_result_reconciliation(corrective: bool, empty: bool) {
        use crate::agent_tree::{MemoryTreeStore, ProductAgentTree, TreeStore as _};
        use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest};
        use plakous::sessions::{AgentSessions, SessionBackend};
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, runtime_binding};

        let root = tempfile::tempdir().unwrap();
        let tree_store = Arc::new(MemoryTreeStore::new());
        let runtime = ProductTopologyRuntime::new(
            crate::topology_runtime::test_support::bootstrap(
                "result-recovery",
                root.path().to_owned(),
            ),
            tree_store.clone(),
            Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
            None,
        );
        let parent = runtime
            .mutate_then_emit(false, |tree| {
                let mut snapshot = tree.snapshot.clone();
                snapshot.max_depth = 2;
                snapshot.digest = snapshot.compute_digest();
                tree.install_snapshot(snapshot);
                let parent = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                    .unwrap();
                let child = tree
                    .spawn_agent(&parent.agent_id, &SpawnAgentRequest::test_review(), None)
                    .unwrap();
                tree.submit_result(
                    &child.agent_id,
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: r#"{"ok":true}"#.to_owned(),
                    },
                    None,
                )
                .unwrap();
                tree.record_turn_completed(&child.agent_id).unwrap();
                Ok(parent)
            })
            .unwrap();
        if empty {
            runtime
                .mutate_then_emit(false, |tree| {
                    let ids = tree
                        .pending_result_mail(&parent.agent_id)
                        .into_iter()
                        .map(|mail| mail.message_id.clone())
                        .collect::<Vec<_>>();
                    for id in ids {
                        tree.acknowledge_mail(&id).map_err(|error| error.message)?;
                    }
                    Ok(())
                })
                .unwrap();
        }
        let evidence = root.path().join("result-recovery.jsonl");
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        let database = root.path().join("result-recovery.sqlite3");
        let sessions =
            Arc::new(AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap());
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("result-recovery-create").unwrap(),
                runtime_binding(root.path(), controller.clone()).unwrap(),
            ))
            .await
            .unwrap()
        else {
            panic!("new session required")
        };
        let prior_turn = if corrective {
            let prior = session
                .agent()
                .start(CommandId::new("result-recovery-prior").unwrap(), "prior")
                .await
                .unwrap();
            let result = prior.result().unwrap();
            result.clone().result().await.unwrap();
            Some(result.turn_id())
        } else {
            None
        };
        // A corrective round's identity names its attempt and round.
        let command = if corrective {
            let attempt = runtime
                .read(|tree| tree.attempt(&parent.agent_id).unwrap().attempt_id.clone())
                .unwrap();
            format!("plak:submit-harness:{}:1:{attempt}:1", parent.agent_id)
        } else {
            "result-recovery-start".to_owned()
        };
        let context = super::super::result_context::prepare(
            runtime.driver(),
            &parent.agent_id,
            &command,
            "continue",
        )
        .unwrap();
        let submission = session
            .agent()
            .start(CommandId::new(command.clone()).unwrap(), context.prompt)
            .await
            .unwrap();
        let result = submission.result().unwrap().clone();
        let turn = result.turn_id();
        result.result().await.unwrap();
        // Durable cut: runtime admitted and tree bound the turn, but the
        // result-context acknowledgement has never run.
        runtime
            .mutate_then_emit(false, |tree| {
                tree.bind_attempt_turn(&parent.agent_id, prior_turn.unwrap_or(turn).to_string())
                    .map_err(|error| error.message)
            })
            .unwrap();
        let locator = session.release().await.unwrap();
        drop(sessions);
        let persisted = tree_store.load("result-recovery").unwrap().unwrap();
        drop(runtime);
        let runtime = ProductTopologyRuntime::new(
            ProductAgentTree::reopen(persisted),
            tree_store,
            Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
            None,
        );
        let before = std::fs::read(&evidence).unwrap();
        let sessions =
            Arc::new(AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap());
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("result-recovery-open").unwrap(),
                locator,
                runtime_binding(root.path(), controller).unwrap(),
            ))
            .await
            .unwrap()
        else {
            panic!("quiescent reopened session required")
        };
        let reopened = reconcile_reopened_child_results(
            &runtime,
            &parent.agent_id,
            ChildReopen::Attached {
                session: Box::new(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: None,
                    sessions,
                    locator: session.locator(),
                    opened: Some(session),
                    recovered_turn: None,
                    topology_owner: None,
                }),
                recovered: None,
            },
            ChildReopenMode::OWNER,
        )
        .await
        .unwrap();
        assert!(!bound_child_turn_matches(&runtime, &parent.agent_id, turn));
        if corrective {
            assert_eq!(
                runtime
                    .read(|tree| tree
                        .attempt(&parent.agent_id)
                        .unwrap()
                        .submit_harness_nudge
                        .rounds())
                    .unwrap(),
                1
            );
        }
        assert_eq!(
            std::fs::read(&evidence).unwrap(),
            before,
            "reconciliation cannot dispatch provider work"
        );
        assert!(
            runtime
                .read(|tree| tree.pending_result_mail(&parent.agent_id).is_empty())
                .unwrap()
        );
        assert!(
            super::super::result_context::prepare(
                runtime.driver(),
                &parent.agent_id,
                "result-recovery-next",
                "next"
            )
            .is_ok(),
            "recorded acceptance must not leave later commands permanently blocked"
        );
        let ChildReopen::Attached { session, .. } = reopened else {
            panic!("attached session required")
        };
        session.release_opened().await.unwrap();
    }

    #[tokio::test]
    async fn fresh_child_start_retains_control_after_terminal_before_ack() {
        child_start_control_fixture(false, true, false).await;
    }

    #[tokio::test]
    async fn duplicate_child_start_registers_existing_live_turn_without_redispatch() {
        child_start_control_fixture(true, false, false).await;
    }

    #[tokio::test]
    async fn duplicate_child_start_without_live_turn_requires_reconciliation() {
        child_start_control_fixture(true, true, false).await;
    }

    #[tokio::test]
    async fn duplicate_child_start_below_retention_preserves_its_projected_acceptance() {
        child_start_control_fixture(true, true, true).await;
    }

    #[tokio::test]
    async fn reclaimed_child_start_settles_consumed_without_binding() {
        use plakous::model::ModelErrorKind;
        use plakous::sessions::{AgentSessions, PruneHistoryOutcome, SessionBackend};
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let child = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent(
                        "root",
                        &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                        None,
                    )
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        let evidence = root.path().join("reclaimed-start.jsonl");
        let binding = || {
            terminal_failure_runtime_binding(
                root.path(),
                BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                    .unwrap(),
                ModelErrorKind::Protocol,
            )
            .unwrap()
        };
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("reclaimed.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("reclaimed:create").unwrap(),
                binding(),
            ))
            .await
            .unwrap()
        else {
            panic!("created session required");
        };
        let command = CommandId::new("reclaimed:start").unwrap();
        session
            .agent()
            .start(command.clone(), "work")
            .await
            .unwrap()
            .result()
            .unwrap()
            .clone()
            .result()
            .await
            .unwrap();
        for round in 0..17 {
            session
                .agent()
                .start(CommandId::new(format!("prune:{round}")).unwrap(), "work")
                .await
                .unwrap()
                .result()
                .unwrap()
                .clone()
                .result()
                .await
                .unwrap();
        }
        let locator = session.release().await.unwrap();
        assert!(matches!(
            sessions.prune_history(locator).await.unwrap(),
            PruneHistoryOutcome::Pruned(_)
        ));
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("reclaimed:open").unwrap(),
                locator,
                binding(),
            ))
            .await
            .unwrap()
        else {
            panic!("pruned session must reopen");
        };
        let replay = session
            .agent()
            .command(plakous::agent::TurnCommand::Start {
                command_id: command.clone(),
                input: "work".to_owned().into(),
            })
            .await
            .unwrap_or_else(|error| panic!("reclaimed replay: {}", error.message()));
        assert_eq!(
            replay
                .acknowledgement()
                .rejection()
                .map(plakous::agent::ControlError::kind),
            Some(plakous::agent::ControlErrorKind::Reclaimed)
        );
        let attached = AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions,
            locator: session.locator(),
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        };
        let startup = runtime.reserve_startup(&child).await.unwrap();
        let Err(ChildStartFailure::Failed(message)) = start_child_turn_owned_with_ack(
            &runtime,
            &child,
            attached,
            command.clone(),
            "work",
            &startup,
            true,
            StartAdmission::Ordinary,
            std::future::ready(()),
        )
        .await
        else {
            panic!("a reclaimed command starts nothing");
        };
        assert_eq!(message, CHILD_COMMAND_CONSUMED);
        runtime
            .read(|tree| {
                let intent = tree.command(command.as_str()).unwrap();
                assert_eq!(intent.fact, Some(crate::agent_tree::CommandFact::Accepted));
                assert!(!intent.withdrawn);
                assert!(intent.turn_id.is_none());
                assert!(tree.attempt(&child).unwrap().turn_id.is_none());
                assert_eq!(tree.attempt(&child).unwrap().usage.turns, 0);
                assert!(!tree.node(&child).unwrap().active_turn);
            })
            .unwrap();
    }

    async fn child_start_control_fixture(
        duplicate: bool,
        terminal_before_ack: bool,
        below_retention: bool,
    ) {
        use crate::agent_tree_protocol::{AttemptStatus, NodeStatus, SpawnAgentRequest, Tier};
        use plakous::model::ModelErrorKind;
        use plakous::sessions::{AgentSessions, SessionBackend};
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let id = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent(
                        "root",
                        &SpawnAgentRequest::test_named(
                            "implement",
                            Tier::Mutate,
                            "implement",
                            "work",
                        ),
                        None,
                    )
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        let evidence = root.path().join("start-control-evidence.jsonl");
        let release = root.path().join("release-provider");
        let controller = BoundaryController::quiet_gated_from_evidence_path(
            CrashBoundary::ProviderDispatchOne,
            &evidence,
            &release,
        )
        .unwrap();
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("start-control.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("start-control-create").unwrap(),
                terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                    .unwrap(),
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        let agent = session.agent().clone();
        let command = CommandId::new("start-control-command").unwrap();
        super::super::result_context::prepare(runtime.driver(), &id, command.as_str(), "work")
            .unwrap();
        if duplicate {
            let first = agent.start(command.clone(), "work").await.unwrap();
            assert!(first.turn().is_some());
        }
        let attached = AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions,
            locator: session.locator(),
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        };
        let startup = runtime.reserve_startup(&id).await.unwrap();
        let started = start_child_turn_owned_with_ack(
            &runtime,
            &id,
            attached,
            command.clone(),
            "work",
            &startup,
            true,
            StartAdmission::Ordinary,
            async {
                tokio::time::timeout(Duration::from_secs(20), async {
                    while !plakous_offline_crash_harness::evidence::read_evidence(&evidence)
                        .is_ok_and(|rows| {
                            rows.iter()
                                .any(|row| row.boundary == CrashBoundary::ProviderDispatchOne)
                        })
                    {
                        tokio::task::yield_now().await;
                    }
                    if terminal_before_ack {
                        let replay = agent.start(command.clone(), "work").await.unwrap();
                        assert_eq!(
                            replay.acknowledgement().disposition(),
                            plakous::agent::CommandAckDisposition::Duplicate
                        );
                        assert!(replay.turn().is_none());
                        let result = replay.result().unwrap().clone();
                        let turn = result.turn_id();
                        std::fs::write(&release, "release").unwrap();
                        assert!(matches!(
                            result.result().await.unwrap().outcome(),
                            TurnResultOutcome::Failed(_)
                        ));
                        while agent.turn_control(turn).is_some() {
                            tokio::task::yield_now().await;
                        }
                        if below_retention {
                            let connection = rusqlite::Connection::open(root.path().join("start-control.sqlite3")).unwrap();
                            connection.execute_batch(
                                "DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
                                 UPDATE sessions SET feed_retained_from=feed_tail,\
                                 feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);",
                            ).unwrap();
                        }
                    }
                })
                .await
                .unwrap();
            },
        )
        .await;
        if below_retention {
            let Err(ChildStartFailure::Failed(message)) = started else {
                panic!("a duplicate below retention starts nothing")
            };
            assert!(
                message.contains("duplicate child start has no live turn"),
                "a retained view, not the acknowledgement sequence, settles the duplicate: {message}"
            );
            runtime
                .read(|tree| {
                    let intent = tree.command(command.as_str()).unwrap();
                    assert_eq!(intent.fact, Some(crate::agent_tree::CommandFact::Accepted));
                    assert!(!intent.withdrawn);
                    // The startup's initial feed pass already observed the
                    // original admission, before the fixture pruned its feed.
                    assert!(intent.turn_id.is_some());
                    assert!(tree.attempt(&id).unwrap().turn_id.is_none());
                    assert_eq!(tree.attempt(&id).unwrap().usage.turns, 1);
                    assert!(!tree.node(&id).unwrap().active_turn);
                })
                .unwrap();
        } else if duplicate && terminal_before_ack {
            let Err(ChildStartFailure::Failed(message)) = started else {
                panic!("retired duplicate must require reconciliation")
            };
            assert!(
                message.contains("duplicate child start has no live turn"),
                "{message}"
            );
            super::super::result_context::prepare(runtime.driver(), &id, command.as_str(), "work")
                .unwrap();
            runtime
                .read(|tree| {
                    let intent = tree.command(command.as_str()).unwrap();
                    assert_eq!(intent.fact, Some(crate::agent_tree::CommandFact::Accepted));
                    assert!(intent.turn_id.is_some());
                    assert!(!tree.node(&id).unwrap().active_turn);
                    assert!(tree.attempt(&id).unwrap().turn_id.is_none());
                    assert!(!tree.has_turn_owner(&id));
                })
                .unwrap();
        } else {
            let turn = started.unwrap_or_else(|failure| panic!("{}", String::from(failure)));
            assert_eq!(
                runtime
                    .read(|tree| tree.command(command.as_str()).unwrap().turn_id.clone())
                    .unwrap(),
                Some(turn.to_string())
            );
            if !terminal_before_ack {
                std::fs::write(&release, "release").unwrap();
            }
        }
        tokio::time::timeout(Duration::from_secs(20), async {
            while runtime.live.lock().await.contains_key(&id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&evidence)
                .unwrap()
                .lines()
                .filter(
                    |line| serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                        == "provider_dispatched"
                )
                .count(),
            1,
            "exact command replay cannot redispatch"
        );
        if !duplicate || !terminal_before_ack {
            runtime
                .read(|tree| {
                    assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Failed);
                    assert!(!tree.node(&id).unwrap().active_turn);
                    assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Failed);
                    assert_eq!(
                        tree.attempt(&id).unwrap().failure.as_ref().unwrap().kind,
                        "turn_failed"
                    );
                    assert_eq!(tree.pending_result_mail("root").len(), 1);
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn real_session_late_start_ack_releases_after_deadline_without_redispatch() {
        use crate::agent_tree_protocol::{
            AttemptScopedRequest, NodeStatus, SpawnAgentRequest, Tier,
        };
        use plakous::model::ModelErrorKind;
        use plakous::sessions::{AgentSessions, SessionBackend};
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let id = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent(
                        "root",
                        &SpawnAgentRequest::test_named(
                            "implement",
                            Tier::Mutate,
                            "implement",
                            "work",
                        ),
                        None,
                    )
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        let evidence = root.path().join("startup-evidence.jsonl");
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        let binding = || {
            terminal_failure_runtime_binding(
                root.path(),
                controller.clone(),
                ModelErrorKind::Protocol,
            )
            .unwrap()
        };
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("startup.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("startup-real-create").unwrap(),
                binding(),
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        let original_locator = session.locator();
        let attached = AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: sessions.clone(),
            opened: Some(session),
            recovered_turn: None,
            locator: original_locator,
            topology_owner: None,
        };
        let startup = runtime.reserve_startup(&id).await.unwrap();
        let caller = StartupCaller::new(runtime.clone(), id.clone(), startup.clone());
        let entered = Arc::new(tokio::sync::Notify::new());
        let deliver_ack = Arc::new(tokio::sync::Notify::new());
        let worker_runtime = runtime.clone();
        let worker_id = id.clone();
        let gate_entered = entered.clone();
        let gate_release = deliver_ack.clone();
        let worker = tokio::spawn(async move {
            start_child_turn_owned_with_ack(
                &worker_runtime,
                &worker_id,
                attached,
                CommandId::new("startup-real-command").unwrap(),
                "run exactly once",
                &startup,
                true,
                StartAdmission::Ordinary,
                async move {
                    gate_entered.notify_one();
                    gate_release.notified().await;
                },
            )
            .await
        });
        entered.notified().await;
        let dispatches = || {
            std::fs::read_to_string(&evidence)
                .unwrap()
                .lines()
                .filter(|line| {
                    serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                        == "provider_dispatched"
                })
                .count()
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while dispatches() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // The actual command was admitted and dispatched; only acknowledgement
        // delivery is withheld from the host startup owner.
        assert_eq!(dispatches(), 1);
        let refused = await_child_start(caller, worker, tokio::time::Instant::now())
            .await
            .unwrap_err();
        assert!(String::from(refused).contains("deadline"));
        runtime
            .mutate_then_emit(false, |tree| {
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
                assert!(tree.has_turn_owner(&id));
                assert!(!tree.node(&id).unwrap().active_turn);
                assert!(tree.attempt(&id).unwrap().turn_id.is_none());
                assert!(tree.attempt(&id).unwrap().candidate_id.is_none());
                assert_eq!(tree.revision, tree.stored_revision);
                tree.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: id.clone(),
                        expected_attempt_id: None,
                    },
                    None,
                )
                .unwrap();
                assert!(tree.attempt(&id).unwrap().candidate_id.is_none());
                Ok(())
            })
            .unwrap();
        assert!(runtime.live.lock().await.contains_key(&id));
        deliver_ack.notify_one();
        tokio::time::timeout(Duration::from_secs(3), async {
            while runtime.live.lock().await.contains_key(&id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let released = runtime
            .read(|tree| {
                assert!(!tree.node(&id).unwrap().active_turn);
                assert!(tree.attempt(&id).unwrap().turn_id.is_none());
                assert_eq!(tree.attempt(&id).unwrap().usage.turns, 1);
                assert!(tree.attempt(&id).unwrap().candidate_id.is_none());
                assert_eq!(tree.revision, tree.stored_revision);
                tree.node(&id).unwrap().session_locator.clone().unwrap()
            })
            .unwrap();
        assert_ne!(released, original_locator.to_string());
        let opened = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("startup-real-reopen").unwrap(),
                SessionLocator::from_str(&released).unwrap(),
                binding(),
            ))
            .await
            .unwrap();
        let OpenSessionOutcome::Opened { session, .. } = opened else {
            panic!("released child must reopen without an active turn")
        };
        session.release().await.unwrap();
        assert_eq!(dispatches(), 1, "reopen never dispatches a replacement");
    }

    /// Moves the store's retained-checkpoint counter to where ordinary work
    /// stops. The store is already open, so the counter is read as recorded.
    fn exhaust_ordinary_checkpoints(database: &Path) {
        use plakous::sessions::{SessionBackend, SessionStoreBound};

        let usage = SessionBackend::inspect_sqlite_usage(database)
            .unwrap()
            .unwrap();
        let checkpoints = usage
            .bounds()
            .iter()
            .find(|bound| bound.bound() == SessionStoreBound::Checkpoints)
            .unwrap();
        rusqlite::Connection::open(database)
            .unwrap()
            .execute(
                "UPDATE store_meta SET checkpoints=?1 WHERE id=1",
                [i64::try_from(checkpoints.ordinary_threshold()).unwrap()],
            )
            .unwrap();
    }

    fn retained_sessions(database: &Path) -> i64 {
        rusqlite::Connection::open(database)
            .unwrap()
            .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
            .unwrap()
    }

    /// A child-session create refused for store capacity creates nothing, so
    /// the spawn is a definite refusal rather than a recovery-required child.
    #[tokio::test]
    async fn a_child_create_refused_under_storage_pressure_is_a_definite_spawn_refusal() {
        use plakous::model::ModelErrorKind;
        use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionBackend};
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join("create-pressure-evidence.jsonl");
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        let database = root.path().join("create-pressure.sqlite3");
        let sessions = AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap();
        exhaust_ordinary_checkpoints(&database);
        let Err(error) = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("create-under-pressure").unwrap(),
                terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                    .unwrap(),
            ))
            .await
        else {
            panic!("a create was admitted without ordinary headroom");
        };
        assert_eq!(error.kind(), AgentSessionsErrorKind::StoragePressure);
        assert_eq!(
            ChildReopenError::from_sessions("recovery create", &error).kind,
            ChildReopenErrorKind::StorageUnavailable,
            "recovery must surface capacity as storage unavailable"
        );
        let SpawnBindError::Refused(message) = child_create_failure(&error, false) else {
            panic!("a capacity-refused create was left for recovery");
        };
        assert!(message.contains("session store is full"), "{message}");
        assert_eq!(
            retained_sessions(&database),
            0,
            "a child session was created"
        );
    }

    /// A create refused for a held lease is transient, like storage
    /// unavailability: even when the accepted-create lookup finds no session
    /// it does not prove the spawn never produced one, so the spawn stays
    /// recovery-required rather than a definite `spawn_refused`, and recovery
    /// keeps it retryable.
    #[tokio::test]
    async fn a_child_create_refused_for_a_held_lease_is_not_a_definite_spawn_refusal() {
        use plakous::model::ModelErrorKind;
        use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionBackend};
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join("create-lease-evidence.jsonl");
        let binding = || {
            let controller =
                BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                    .unwrap();
            terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                .unwrap()
        };
        let sessions = AgentSessions::new(
            SessionBackend::sqlite(root.path().join("create-lease.sqlite3")).unwrap(),
        )
        .unwrap();
        let operation = SessionOperationId::new("create-under-held-lease").unwrap();
        let Ok(CreateSessionOutcome::Created { session, .. }) = sessions
            .create(CreateSessionRequest::new(operation.clone(), binding()))
            .await
        else {
            panic!("the first create creates its session");
        };
        let Err(error) = sessions
            .create(CreateSessionRequest::new(operation, binding()))
            .await
        else {
            panic!("a create was admitted while its operation is still attached");
        };
        assert_eq!(error.kind(), AgentSessionsErrorKind::Ownership);
        assert!(
            matches!(
                child_create_failure(&error, true),
                SpawnBindError::RecoveryRequired { .. }
            ),
            "a lease refusal settled the spawn as refused"
        );
        assert!(
            !ChildReopenError::from_sessions("recovery create", &error)
                .kind
                .settles_never_created()
        );
        session.release().await.unwrap();
    }

    async fn child_start_refusal_under_pressure(delete_fresh_session: bool, cancel_at_ack: bool) {
        use crate::agent_tree_protocol::{NodeStatus, SpawnAgentRequest, Tier};
        use plakous::model::ModelErrorKind;
        use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionBackend};
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};

        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let id = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent(
                        "root",
                        &SpawnAgentRequest::test_named(
                            "implement",
                            Tier::Mutate,
                            "implement",
                            "work",
                        ),
                        None,
                    )
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        let evidence = root.path().join("start-pressure-evidence.jsonl");
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        let database = root.path().join("start-pressure.sqlite3");
        let sessions =
            Arc::new(AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap());
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("start-pressure-create").unwrap(),
                terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                    .unwrap(),
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        let locator = session.locator();
        assert_eq!(retained_sessions(&database), 1);
        exhaust_ordinary_checkpoints(&database);
        let attached = AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: sessions.clone(),
            opened: Some(session),
            recovered_turn: None,
            locator,
            topology_owner: None,
        };
        let startup = runtime.reserve_startup(&id).await.unwrap();
        let cancellation = startup.clone();
        let Err(failure) = start_child_turn_owned_with_ack(
            &runtime,
            &id,
            attached,
            CommandId::new("start-under-pressure").unwrap(),
            "never runs",
            &startup,
            delete_fresh_session,
            StartAdmission::Ordinary,
            async move {
                if cancel_at_ack {
                    cancellation.request_cancel();
                }
            },
        )
        .await
        else {
            panic!("a child start was admitted without ordinary headroom");
        };
        assert_eq!(
            matches!(failure, ChildStartFailure::Refused(_)),
            !cancel_at_ack,
            "late cancellation must retain uncertain genesis instead of claiming definite cleanup"
        );
        assert!(
            !std::fs::read_to_string(&evidence)
                .unwrap_or_default()
                .contains("provider_dispatched"),
            "a refused start reached the provider"
        );
        assert_eq!(
            retained_sessions(&database),
            i64::from(!delete_fresh_session || cancel_at_ack),
            "child-session retention disagreed with startup ownership"
        );
        if delete_fresh_session && !cancel_at_ack {
            assert_eq!(
                sessions.archive(locator).await.unwrap_err().kind(),
                AgentSessionsErrorKind::NotFound
            );
        } else {
            let archived = sessions.archive(locator).await.unwrap();
            sessions
                .delete(archived.session_id(), archived.archived_revision())
                .await
                .unwrap();
        }
        runtime
            .read(|tree| {
                assert_ne!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
                assert!(!tree.node(&id).unwrap().active_turn);
            })
            .unwrap();
    }

    /// A fresh spawn owns its never-started session and may delete it after a
    /// definite capacity refusal.
    #[tokio::test]
    async fn a_child_start_refused_under_storage_pressure_is_definite_and_leaves_no_session() {
        child_start_refusal_under_pressure(true, false).await;
    }

    /// A mail-delivered Start is reusing an existing child. Its definite
    /// refusal rolls the attempt back without deleting that child's history.
    #[tokio::test]
    async fn an_existing_child_start_refusal_preserves_its_session() {
        child_start_refusal_under_pressure(false, false).await;
    }

    /// Cancellation can arrive after the rejection acknowledgement but before
    /// release. It wins the atomic cleanup claim, so the fresh genesis is kept
    /// for explicit later GC rather than being archived underneath the caller.
    #[tokio::test]
    async fn cancellation_at_cleanup_retains_uncertain_fresh_genesis() {
        child_start_refusal_under_pressure(true, true).await;
    }

    #[tokio::test]
    async fn aborted_startup_worker_frees_its_unpolled_reservation() {
        use crate::agent_tree_protocol::NodeStatus;

        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let startup = runtime.reserve_startup("root").await.unwrap();
        let worker = StartupWorker::new(runtime.clone(), "root".into(), startup);
        let notified = Arc::new(tokio::sync::Notify::new());
        let trigger = notified.clone();
        runtime
            .set_retirement_recovery(Arc::new(move || trigger.notify_one()))
            .await;
        let task = tokio::spawn(async move {
            let _worker = worker;
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), notified.notified())
            .await
            .unwrap();
        assert!(!runtime.live.lock().await.contains_key("root"));
        runtime
            .read(|tree| {
                assert_eq!(
                    tree.node("root").unwrap().status,
                    NodeStatus::RecoveryRequired
                );
                assert_eq!(
                    tree.fault_reason("root"),
                    Some("child startup owner ended without finishing")
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn startup_worker_fallback_does_not_fault_a_transferred_owner() {
        use crate::agent_tree_protocol::NodeStatus;

        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let startup = runtime.reserve_startup("root").await.unwrap();
        let mut worker = StartupWorker::new(runtime.clone(), "root".into(), startup.clone());
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        finish.notify_one();
        assert!(
            runtime
                .occupy_started_child(
                    "root".into(),
                    &startup,
                    Box::new(EndingChild {
                        turn: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                        entered,
                        finish,
                        failure: false,
                    })
                )
                .await
                .is_ok()
        );
        worker.finish().await;
        runtime
            .read(|tree| {
                assert_eq!(tree.node("root").unwrap().status, NodeStatus::Running);
                assert!(tree.fault_reason("root").is_none());
            })
            .unwrap();
        crate::topology_runtime::retire_live(runtime.driver(), &runtime.live, "root", None, false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn abandoned_startup_caller_cancels_late_registered_turn_once() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let startup = runtime.reserve_startup("root").await.unwrap();
        let caller = StartupCaller::new(runtime.clone(), "root".into(), startup.clone());
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        assert!(
            runtime
                .occupy_started_child(
                    "root".into(),
                    &startup,
                    Box::new(CountedCancellation {
                        child: EndingChild {
                            turn,
                            entered: entered.clone(),
                            finish: finish.clone(),
                            failure: false
                        },
                        calls: calls.clone(),
                    })
                )
                .await
                .is_ok()
        );
        // Registration won just before the caller's deadline. Cancellation
        // must target the transferred live owner, not an empty reservation.
        drop(caller);
        entered.notified().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(runtime.live.lock().await.contains_key("root"));
        finish.notify_one();
        runtime.cancel_live("root").await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(runtime.live.lock().await.is_empty());
    }

    #[tokio::test]
    async fn cancelled_startup_rejects_late_ack_owner_without_dropping_it() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let startup = runtime.reserve_startup("root").await.unwrap();
        let caller = StartupCaller::new(runtime.clone(), "root".into(), startup.clone());
        drop(caller);
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rejected = runtime
            .occupy_started_child(
                "root".into(),
                &startup,
                Box::new(CountedCancellation {
                    child: EndingChild {
                        turn,
                        entered: entered.clone(),
                        finish: finish.clone(),
                        failure: false,
                    },
                    calls: calls.clone(),
                }),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(rejected.turn_id(), turn);
        let release_driver = runtime.driver().clone();
        let release =
            tokio::spawn(async move { rejected.cancel_and_release(release_driver).await });
        entered.notified().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        finish.notify_one();
        runtime
            .finish_startup("root", &startup, release.await.unwrap())
            .await;
        assert!(runtime.live.lock().await.is_empty());
    }

    #[tokio::test]
    async fn completion_reservation_survives_concurrent_cancel_and_reopen() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let child = || {
            Box::new(EndingChild {
                turn,
                entered: entered.clone(),
                finish: finish.clone(),
                failure: false,
            }) as Box<dyn LiveChildHandle>
        };
        assert!(
            runtime
                .try_occupy_live("root".into(), child())
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let retired = tokio::spawn(async move {
            retire_live_child(owner.driver(), &owner.live, "root", turn).await;
        });
        entered.notified().await;
        assert!(runtime.live.try_lock().is_ok());
        let mut cancel = Box::pin(runtime.cancel_live("root"));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                cancel.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        assert!(
            runtime
                .try_occupy_live("root".into(), child())
                .await
                .is_err()
        );
        finish.notify_one();
        retired.await.unwrap();
        assert_eq!(
            cancel.await.unwrap(),
            crate::topology_runtime::LiveCancelOutcome::Cancelled
        );
        assert!(runtime.live.lock().await.is_empty());
        runtime
            .read(|tree| {
                assert_eq!(tree.node("root").unwrap().session_locator.as_deref(), None);
                assert_eq!(tree.revision, tree.stored_revision);
            })
            .unwrap();
    }
    #[tokio::test]
    async fn failed_normal_release_clears_slot_and_fails_joined_close_canceller() {
        use crate::agent_tree_tools::LiveTurnCanceller;
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let child_id = runtime
            .mutate_then_emit(false, |tree| {
                Ok(tree
                    .spawn_agent(
                        "root",
                        &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                        None,
                    )
                    .unwrap()
                    .agent_id)
            })
            .unwrap();
        assert!(
            runtime
                .try_occupy_live(
                    child_id.clone(),
                    Box::new(EndingChild {
                        turn,
                        entered: entered.clone(),
                        finish: finish.clone(),
                        failure: true,
                    })
                )
                .await
                .is_ok()
        );
        let owner = runtime.clone();
        let id = child_id.clone();
        let retired = tokio::spawn(async move {
            retire_live_child(owner.driver(), &owner.live, &id, turn).await;
        });
        entered.notified().await;
        let canceller = ProductLiveCanceller {
            runtime: runtime.clone(),
        };
        let mut close = canceller.cancel_agents(vec![child_id.clone()]);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(close.as_mut().poll(cx).is_pending()))
                .await
        );
        retired.abort();
        assert!(retired.await.unwrap_err().is_cancelled());
        finish.notify_one();
        let failure = close.await.unwrap_err();
        assert_eq!(failure.failed_ids, vec![child_id.clone()]);
        assert_eq!(failure.not_live_ids, [] as [String; 0]);
        assert_eq!(failure.message, "release refused");
        assert_eq!(
            runtime.cancel_live(&child_id).await.unwrap(),
            crate::topology_runtime::LiveCancelOutcome::NotLive
        );
        assert!(!runtime.live.lock().await.contains_key(&child_id));
        runtime
            .read(|tree| {
                assert_eq!(tree.fault_reason(&child_id), Some("release refused"));
                assert_eq!(tree.revision, tree.stored_revision);
                assert!(tree.node(&child_id).unwrap().session_locator.is_none());
            })
            .unwrap();
    }
    struct CountedCancellation {
        child: EndingChild,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl LiveChildHandle for CountedCancellation {
        fn agent(&self) -> Option<plakous::agent::Agent> {
            None
        }
        fn turn_id(&self) -> TurnId {
            self.child.turn
        }
        fn turn_control(&self) -> TurnControl {
            panic!("fixture")
        }
        fn follow_up(
            &self,
            _: CommandId,
            _: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
            Box::pin(async { Err("fixture".into()) })
        }
        fn restart(
            &mut self,
            _: TreeDriver,
            _: CommandId,
            _: String,
        ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
            Box::pin(async { Err("fixture".into()) })
        }
        fn release(
            self: Box<Self>,
            driver: TreeDriver,
        ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
            Box::new(self.child).release(driver)
        }
        fn cancel_and_release(
            self: Box<Self>,
            driver: TreeDriver,
        ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::new(self.child).release(driver)
        }
    }

    #[tokio::test]
    async fn exact_operation_replay_never_claims_pending_cancel_or_cancels_replacement() {
        use plakous::tools::ToolOutcomeKind;
        use std::sync::atomic::{AtomicUsize, Ordering};
        for name in ["interrupt_agent", "close_agent"] {
            for (failed_release, abandon_caller) in [(false, false), (true, false), (false, true)] {
                let root = tempfile::tempdir().unwrap();
                let runtime = runtime(root.path());
                let turn = TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
                let child_id = runtime
                    .mutate_then_emit(false, |tree| {
                        let child = tree
                            .spawn_agent(
                                "root",
                                &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                                None,
                            )
                            .unwrap()
                            .agent_id;
                        tree.set_active_turn(&child, true).unwrap();
                        tree.bind_attempt_turn(&child, turn.to_string()).unwrap();
                        Ok(child)
                    })
                    .unwrap();
                let entered = Arc::new(tokio::sync::Notify::new());
                let finish = Arc::new(tokio::sync::Notify::new());
                let calls = Arc::new(AtomicUsize::new(0));
                assert!(
                    runtime
                        .try_occupy_live(
                            child_id.clone(),
                            Box::new(CountedCancellation {
                                child: EndingChild {
                                    turn,
                                    entered: entered.clone(),
                                    finish: finish.clone(),
                                    failure: failed_release
                                },
                                calls: calls.clone(),
                            })
                        )
                        .await
                        .is_ok()
                );
                let arguments = serde_json::json!({ "agent_id": child_id });
                let invoke = || {
                    crate::agent_tree_tools::invoke_granted_topology_tool_on(
                        runtime.driver().clone(),
                        "root".into(),
                        name,
                        &arguments,
                        "exact-cancel".into(),
                        crate::agent_tree_tools::TopologyHostPorts {
                            canceller: Some(Arc::new(ProductLiveCanceller {
                                runtime: runtime.clone(),
                            })),
                            ..Default::default()
                        },
                    )
                };
                let mut original = Box::pin(invoke());
                assert!(
                    std::future::poll_fn(|cx| std::task::Poll::Ready(
                        original.as_mut().poll(cx).is_pending()
                    ))
                    .await
                );
                entered.notified().await;
                let pending = invoke().await.unwrap();
                assert_eq!(pending.kind(), ToolOutcomeKind::Failure);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                runtime
                    .read(|tree| {
                        let receipt = &tree.operations()["exact-cancel"];
                        assert!(receipt.terminal_response.is_none());
                        assert!(receipt.terminal_code.is_none());
                    })
                    .unwrap();
                if abandon_caller {
                    drop(original);
                    finish.notify_one();
                    assert!(runtime.cancel_live(&child_id).await.is_ok());
                    assert_eq!(invoke().await.unwrap().kind(), ToolOutcomeKind::Failure);
                    assert_eq!(calls.load(Ordering::SeqCst), 1);
                    continue;
                }
                finish.notify_one();
                let expected = if failed_release {
                    ToolOutcomeKind::Failure
                } else {
                    ToolOutcomeKind::Success
                };
                assert_eq!(original.await.unwrap().kind(), expected);
                assert_eq!(invoke().await.unwrap().kind(), expected);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                let replacement = TurnId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
                let occupied = runtime
                    .try_occupy_live(
                        child_id.clone(),
                        Box::new(CountedCancellation {
                            child: EndingChild {
                                turn: replacement,
                                entered: entered.clone(),
                                finish: finish.clone(),
                                failure: false,
                            },
                            calls: calls.clone(),
                        }),
                    )
                    .await;
                assert!(occupied.is_ok(), "returned release no longer owns the slot");
                assert_eq!(invoke().await.unwrap().kind(), expected);
                assert_eq!(
                    runtime.live.lock().await.get(&child_id).unwrap().turn_id(),
                    replacement
                );
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
        }
    }
}

#[cfg(test)]
mod reopen_error_tests {
    use super::*;
    use plakous::sessions::SessionBackend;

    #[test]
    fn an_incompatible_store_reads_as_the_one_incompatible_build_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("runtime.sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .pragma_update(None, "application_id", 0x504c_4b35_i64)
            .unwrap();
        connection
            .pragma_update(None, "user_version", 13_i64)
            .unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .unwrap();
        drop(connection);
        let error = SessionBackend::sqlite(&database).expect_err("an older schema opened");
        assert_eq!(
            format_session_reopen_error("exact session reopen failed", &error),
            "this session was created by an incompatible plak build; start a new session"
        );
    }
}
