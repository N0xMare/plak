//! Binding a spawned child to a real session: creation, startup and the child runtime binding.

use super::{
    AgentSessionsError, ApiTopologyParent, Arc, AttachedCodingSession, ChildAssignmentStartKind,
    ChildReopenErrorKind, ChildStartFailure, ChildStartup, ChildStartupStage, CommandId,
    CompositionSelection, CreateSessionOutcome, CreateSessionRequest, Duration, Future,
    LiveRelease, ModelId, NativeSubscriptionError, OutputBoundDisposition, Path, PathBuf, Pin,
    ProcessingTierDisposition, ProductAgentSpec, SessionOperationId, SessionRuntimeBinding,
    SessionToolFence, SpawnBindError, SpawnParent, coding_session_runtime_binding_for,
    occupancy_session_fence, product_subscription_request_limits, prompt_agent_tier,
    prompt_composition, release_startup, require_reconstructed_coding_projection,
    selected_coding_provider_configuration_with_processing_tier, spawn_child_composition,
    spawn_parent_tree_id, spawn_roster_composition, spawn_roster_processing_tier,
    start_child_turn_owned,
};

pub(in crate::agent) struct ProductChildSessionBinder {
    pub(in crate::agent) parent: SpawnParent,
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
            Ok(())
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
pub(super) struct StartupWorker {
    pub(super) runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
    pub(super) agent_id: String,
    pub(super) startup: ChildStartup,
    armed: bool,
}

impl StartupWorker {
    pub(super) fn new(
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

    pub(super) fn failure() -> LiveRelease {
        LiveRelease {
            feed: None,
            locator: None,
            failure: Some("child startup owner ended without finishing".into()),
        }
    }

    pub(super) async fn finish(&mut self) {
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
pub(super) struct StartupCaller {
    pub(super) runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
    pub(super) agent_id: String,
    pub(super) startup: ChildStartup,
    armed: bool,
}

impl StartupCaller {
    pub(super) fn new(
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
    pub(super) fn disarm(&mut self) {
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
pub(in crate::agent) struct CreatedChild {
    attached: AttachedCodingSession,
    tree_id: String,
    start_kind: ChildAssignmentStartKind,
    attach_started: std::time::Instant,
}

/// Runs on the retained startup owner. A caller deadline never drops a
/// potentially admitted session creation operation.
pub(in crate::agent) async fn create_spawned_child_session(
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
pub(super) fn child_create_failure(
    error: &AgentSessionsError,
    nothing_created: bool,
) -> SpawnBindError {
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
async fn start_spawned_child(
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

pub(in crate::agent) fn spawn_child_operation_id(
    tree_id: &str,
    agent_id: &str,
) -> Result<SessionOperationId, String> {
    // Tact child ids are tree-local (`c1`). The create operation must be too,
    // or a second tree's spawn revalidates the first tree's child session.
    SessionOperationId::new(format!("plak:spawn:{tree_id}:{agent_id}"))
        .map_err(|_| "child spawn operation identity is invalid".to_owned())
}

pub(in crate::agent) fn child_plan_state_authority(
    tree_id: &str,
    agent_id: &str,
) -> Result<String, String> {
    let authority = format!("{tree_id}.{agent_id}");
    let identity = format!("plak.session.{authority}");
    if identity.len() > 72 {
        return Err("child plan-state authority identity exceeds the portable bound".into());
    }
    Ok(authority)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one child bind names its whole exact tuple"
)]
pub(in crate::agent) async fn child_session_runtime_binding(
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
    parent
        .state_roots()
        .child_runtime_factory()
        .bind(super::runtime_factory::ChildRuntimeRequest {
            parent,
            agent_id,
            path,
            account,
            tuple_id,
            effort,
            opening_durable_session,
            fence,
            created_output_bound,
        })
        .await
}

pub(super) async fn production_child_runtime(
    request: super::runtime_factory::ChildRuntimeRequest<'_>,
) -> Result<SessionRuntimeBinding, String> {
    let super::runtime_factory::ChildRuntimeRequest {
        parent,
        agent_id,
        path,
        account,
        tuple_id,
        effort,
        opening_durable_session,
        fence,
        created_output_bound,
    } = request;
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
pub(in crate::agent) async fn openrouter_child_runtime_binding(
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
    Box::pin(crate::agent::build::api_session_runtime_binding(
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
pub(in crate::agent) fn frozen_child_tool_selection(
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

pub(in crate::agent) fn frozen_openrouter_child_pin(
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

pub(in crate::agent) fn openrouter_frozen_pin(
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
        .map_err(|failure| failure.message().to_owned())?
        .without_model_lookup_retries();
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
        .map_err(child_catalog_failure)?;
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
            .map_err(|error| child_provider_failure(error.kind()))?,
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
pub(in crate::agent) fn child_session_workspace_root(
    parent_root: &Path,
    child_path: &Path,
) -> PathBuf {
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

fn child_catalog_failure(error: NativeSubscriptionError) -> String {
    match error {
        NativeSubscriptionError::ModelLookup { .. } => {
            format!("spawn child model catalog could not be read: {error}")
        }
        _ => "spawn child model catalog could not be read".to_owned(),
    }
}

fn child_provider_failure(error: NativeSubscriptionError) -> String {
    match error {
        NativeSubscriptionError::ModelLookup { .. } => {
            format!("spawn child provider could not be resolved: {error}")
        }
        _ => "spawn child provider could not be resolved".to_owned(),
    }
}

#[cfg(test)]
mod lookup_failure_tests {
    #[test]
    fn child_resolution_keeps_lookup_attempt_count_and_cause() {
        let message = super::child_provider_failure(super::NativeSubscriptionError::ModelLookup {
            attempts: 2,
            cause: "operation deadline",
        });
        assert!(
            message.contains("model lookup failed after 2 attempts: operation deadline"),
            "{message}"
        );
    }

    #[test]
    fn child_binding_keeps_lookup_attempt_count_and_cause() {
        let message = super::child_catalog_failure(super::NativeSubscriptionError::ModelLookup {
            attempts: 3,
            cause: "HTTP 503",
        });
        assert!(
            message.contains("model lookup failed after 3 attempts: HTTP 503"),
            "{message}"
        );
    }
}
