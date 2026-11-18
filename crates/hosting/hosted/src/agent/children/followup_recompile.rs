//! Explicit follow-up recompile of provider configuration.
//!
//! A settled child's exact reopen refuses when the frozen presentation cannot
//! hide a changed provider configuration, and failing that reopen closed the
//! child into `recovery_required`. The parent's follow-up is the explicit
//! transition (spec 01 §5.7): when the account, model and template binding are
//! unchanged, recompile at the quiescent checkpoint and continue. Any other
//! binding change, or a refused recompile, refuses the follow-up before an
//! attempt opens.

use std::future::Future;
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use plakous::agent::BindingChange;
use plakous::model::{
    Digest, EffectiveProviderConfigurationId, ModelAdapterProfileId, ModelId, ProtocolId, RouteId,
    SelectedProviderConfigurationId,
};
use plakous::sessions::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, RebindRefused, RebindSessionOutcome,
    RebindSessionRequest, SessionLocator, SessionOperationId, SessionRuntimeBinding,
};

use super::{
    ChildBoundBind, OutputBoundDisposition, ProductMailDeliverer, SessionToolFence, SpawnParent,
    child_session_runtime_binding, occupancy_session_fence, spawn_parent_tree_id,
};

use crate::agent_tree::TopologyError;
use crate::agent_tree_protocol::{NodeStatus, TopologyErrorCode};
use crate::agent_tree_tools::FollowupRecompile;

const NOTICE: &str = "the follow-up recompiled provider configuration onto the current catalog at the quiescent checkpoint; this is not failover";

/// Test cut: the rebind has committed and the tree has not saved the locator.
#[cfg(test)]
pub(crate) static CUT_AFTER_REBIND: AtomicBool = AtomicBool::new(false);

struct RecordedBinding {
    protocol: ProtocolId,
    route: RouteId,
    model: ModelId,
    adapter: ModelAdapterProfileId,
    account: Digest,
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
}

struct ChildFacts {
    agent_id: String,
    path: String,
    locator: String,
    account: Option<String>,
    tuple_id: Option<String>,
    effort: Option<String>,
    fence: SessionToolFence,
    generation: u64,
}

impl ProductMailDeliverer {
    pub(in crate::agent) fn prepare_followup(
        &self,
        caller_id: String,
        agent_id: String,
        acknowledged: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Option<FollowupRecompile>, TopologyError>> + Send + '_>>
    {
        Box::pin(async move {
            self.recompile_or_refuse(&caller_id, &agent_id, acknowledged)
                .await
        })
    }

    async fn recompile_or_refuse(
        &self,
        caller_id: &str,
        agent_id: &str,
        acknowledged: bool,
    ) -> Result<Option<FollowupRecompile>, TopologyError> {
        let Some(facts) = self.idle_facts(caller_id, agent_id, acknowledged)? else {
            return Ok(None);
        };
        let sessions = self
            .parent
            .state_roots()
            .child_runtime_sessions()
            .map_err(|_| refused("plak runtime service is unavailable"))?;
        let mut locator = facts
            .locator
            .parse::<SessionLocator>()
            .map_err(|_| refused("child session locator is invalid"))?;
        locator = line_head_locator(
            &sessions,
            spawn_parent_tree_id(&self.parent),
            &facts,
            locator,
        )
        .await?;
        let bound = match sessions.created_request_output_bound(locator).await {
            Ok(bound) => bound,
            Err(error) => match crate::driver::reopen::stale_head(&error) {
                Some(current) => {
                    locator = current;
                    sessions
                        .created_request_output_bound(locator)
                        .await
                        .map_err(|error| session_refusal(&error))?
                }
                None => return session_read(&error),
            },
        };
        let recorded = recorded_binding(&sessions, locator).await?;
        let frozen = self
            .runtime_binding(&facts, locator, true, Some(bound))
            .await;
        match frozen {
            Ok(runtime) => {
                if let Some(cause) = authority_cause(&recorded, &runtime) {
                    return Err(refused(cause));
                }
                if same_configuration(&recorded, &runtime) {
                    return Ok(moved_locator(&facts, locator));
                }
                self.recompile(&sessions, &facts, locator, runtime).await
            }
            Err(failure) => {
                let current = self.runtime_binding(&facts, locator, false, None).await;
                let Ok(runtime) = current else {
                    return Err(binding_failure(&failure));
                };
                if let Some(cause) = authority_cause(&recorded, &runtime) {
                    return Err(refused(cause));
                }
                match binding_change_of(&failure) {
                    Some(change) if !recompilable(&change) => Err(refused(change.to_string())),
                    Some(_) => self.recompile(&sessions, &facts, locator, runtime).await,
                    None if same_configuration(&recorded, &runtime) => {
                        Ok(moved_locator(&facts, locator))
                    }
                    None => self.recompile(&sessions, &facts, locator, runtime).await,
                }
            }
        }
    }

    fn idle_facts(
        &self,
        caller_id: &str,
        agent_id: &str,
        acknowledged: bool,
    ) -> Result<Option<ChildFacts>, TopologyError> {
        self.runtime.read(|tree| {
            let node = tree.node(agent_id)?;
            if node.parent_agent_id.as_deref() != Some(caller_id) {
                return Err(TopologyError::new(
                    TopologyErrorCode::GrantDenied,
                    "only the recipient's parent may follow up",
                ));
            }
            let locator = node.session_locator.clone();
            let turn_bound = tree
                .attempt(agent_id)
                .and_then(|attempt| attempt.turn_id.as_ref())
                .is_some();
            let idle = match node.status {
                NodeStatus::Completed | NodeStatus::Failed => {
                    !node.active_turn && locator.is_some()
                }
                NodeStatus::Interrupted => {
                    !node.active_turn && locator.is_some() && (!turn_bound || acknowledged)
                }
                _ => false,
            };
            if !idle {
                return Ok(None);
            }
            let bind = ChildBoundBind::of(node);
            Ok(Some(ChildFacts {
                agent_id: agent_id.to_owned(),
                path: node.path.display().to_string(),
                locator: locator.unwrap_or_default(),
                account: bind.account,
                tuple_id: bind.tuple_id,
                effort: bind.effort,
                fence: occupancy_session_fence(node.tier),
                generation: tree.host_generation,
            }))
        })?
    }

    async fn runtime_binding(
        &self,
        facts: &ChildFacts,
        locator: SessionLocator,
        opening_durable_session: bool,
        bound: Option<OutputBoundDisposition>,
    ) -> Result<SessionRuntimeBinding, crate::CliFailure> {
        let parent = if opening_durable_session {
            let sessions = self.parent.state_roots().child_runtime_sessions()?;
            let roots = self
                .parent
                .state_roots()
                .clone()
                .for_exact_reopen(sessions.as_ref(), locator)
                .await?;
            scoped_parent(&self.parent, roots)
        } else {
            self.parent.clone()
        };
        child_session_runtime_binding(
            &parent,
            &facts.agent_id,
            &facts.path,
            facts.account.as_deref(),
            facts.tuple_id.as_deref(),
            facts.effort.as_deref(),
            opening_durable_session,
            facts.fence,
            bound,
            None,
        )
        .await
    }

    async fn recompile(
        &self,
        sessions: &AgentSessions,
        facts: &ChildFacts,
        locator: SessionLocator,
        runtime: SessionRuntimeBinding,
    ) -> Result<Option<FollowupRecompile>, TopologyError> {
        if !sessions
            .head_is_quiescent(locator)
            .await
            .map_err(|error| session_refusal(&error))?
        {
            return Err(refused(
                "the child's session is not at a quiescent checkpoint",
            ));
        }
        let operation_id = recompile_operation_id(
            spawn_parent_tree_id(&self.parent),
            &facts.agent_id,
            facts.generation,
        )?;
        let target = runtime.target_binding();
        let outcome = Box::pin(sessions.rebind(RebindSessionRequest::recompiling_target(
            operation_id,
            locator,
            target,
            runtime,
        )))
        .await
        .map_err(|error| {
            let cause = error
                .binding_change()
                .map_or_else(|| error.to_string(), ToString::to_string);
            if error.kind() == AgentSessionsErrorKind::NotQuiescent {
                refused("the child's session is not at a quiescent checkpoint")
            } else {
                refused(cause)
            }
        })?;
        match outcome {
            RebindSessionOutcome::Rebound { target, .. } => {
                #[cfg(test)]
                if CUT_AFTER_REBIND.load(Ordering::Acquire) {
                    std::future::pending::<()>().await;
                }
                Ok(Some(FollowupRecompile {
                    notice: NOTICE.to_owned(),
                    locator: target.to_string(),
                }))
            }
            RebindSessionOutcome::RecoveryRequired { .. } => Err(refused(
                "the recompiling configuration transition requires recovery",
            )),
            RebindSessionOutcome::Refused { reason, .. } => Err(refused(rebind_refusal(reason))),
            _ => Err(refused(
                "the recompiling configuration transition was refused",
            )),
        }
    }
}

fn scoped_parent(parent: &SpawnParent, roots: crate::agent::ProductStateRoots) -> SpawnParent {
    let mut parent = parent.clone();
    match &mut parent {
        SpawnParent::Native(spec) => spec.state_roots = roots,
        SpawnParent::Api(spec) => spec.state_roots = roots,
    }
    parent
}

async fn recorded_binding(
    sessions: &AgentSessions,
    locator: SessionLocator,
) -> Result<RecordedBinding, TopologyError> {
    let frozen = sessions
        .created_tool_presentation(locator)
        .await
        .map_err(|error| session_refusal(&error))?;
    let configuration = frozen
        .configuration()
        .ok_or_else(|| refused("the child session has no recorded provider configuration"))?;
    let route = configuration.selected().route();
    Ok(RecordedBinding {
        protocol: route.protocol().clone(),
        route: route.route().clone(),
        model: route.requested_model().clone(),
        adapter: configuration.effective().descriptor().profile().clone(),
        account: configuration.effective().authority().account_binding(),
        selected: configuration.selected_identity(),
        effective: configuration.effective_identity(),
    })
}

fn authority_cause(recorded: &RecordedBinding, runtime: &SessionRuntimeBinding) -> Option<String> {
    let target = runtime.target_binding();
    if recorded.protocol != *target.protocol() {
        return Some(format!(
            "model protocol (recorded `{}`, this build `{}`)",
            recorded.protocol,
            target.protocol()
        ));
    }
    if recorded.route != *target.route() {
        return Some(format!(
            "route (recorded `{}`, this build `{}`)",
            recorded.route,
            target.route()
        ));
    }
    if recorded.model != *target.model() {
        return Some(format!(
            "model (recorded `{}`, this build `{}`)",
            recorded.model,
            target.model()
        ));
    }
    if recorded.account != target.account_binding() {
        return Some("account binding".to_owned());
    }
    let adapter = runtime.adapter_profile();
    if recorded.adapter != adapter {
        return Some(format!(
            "model adapter profile (recorded `{}`, this build `{adapter}`)",
            recorded.adapter
        ));
    }
    None
}

fn same_configuration(recorded: &RecordedBinding, runtime: &SessionRuntimeBinding) -> bool {
    let target = runtime.target_binding();
    recorded.selected == target.selected_provider_configuration()
        && recorded.effective == target.effective_provider_configuration()
}

fn rebind_refusal(reason: RebindRefused) -> &'static str {
    match reason {
        RebindRefused::RunningJobsWithoutJobHost { .. } => {
            "running jobs cannot move onto a binding without a job host"
        }
        _ => "the recompiling configuration transition was refused",
    }
}

fn recompilable(change: &BindingChange) -> bool {
    matches!(
        change,
        BindingChange::ProviderConfiguration | BindingChange::ToolPresentation
    )
}

fn moved_locator(facts: &ChildFacts, locator: SessionLocator) -> Option<FollowupRecompile> {
    let head = locator.to_string();
    (head != facts.locator).then(|| FollowupRecompile {
        notice: NOTICE.to_owned(),
        locator: head,
    })
}

fn binding_change_of(failure: &crate::CliFailure) -> Option<BindingChange> {
    let mut source = std::error::Error::source(failure);
    while let Some(error) = source {
        if let Some(sessions) = error.downcast_ref::<AgentSessionsError>() {
            return sessions.binding_change().cloned();
        }
        source = error.source();
    }
    None
}

fn binding_failure(failure: &crate::CliFailure) -> TopologyError {
    match binding_change_of(failure) {
        Some(change) => refused(change.to_string()),
        None => refused(failure.message()),
    }
}

fn session_read(error: &AgentSessionsError) -> Result<Option<FollowupRecompile>, TopologyError> {
    if error.kind() == AgentSessionsErrorKind::NotQuiescent {
        return Err(refused(
            "the child's session is not at a quiescent checkpoint",
        ));
    }
    let code = match error.kind() {
        AgentSessionsErrorKind::StorageBusy
        | AgentSessionsErrorKind::StorageUnavailable
        | AgentSessionsErrorKind::StorageFull
        | AgentSessionsErrorKind::StoragePressure => TopologyErrorCode::RecoveryStorageUnavailable,
        _ => TopologyErrorCode::FollowupBindingChanged,
    };
    let cause = error
        .binding_change()
        .map_or_else(|| error.to_string(), ToString::to_string);
    Err(TopologyError::new(
        code,
        format!("{cause}; spawn a new child from the template"),
    ))
}

fn session_refusal(error: &AgentSessionsError) -> TopologyError {
    let cause = error
        .binding_change()
        .map_or_else(|| error.to_string(), ToString::to_string);
    refused(cause)
}

fn refused(cause: impl std::fmt::Display) -> TopologyError {
    TopologyError::new(
        TopologyErrorCode::FollowupBindingChanged,
        format!("{cause}; spawn a new child from the template"),
    )
}

/// A crash between the rebind commit and the tree save leaves the node on a
/// superseded branch. Reads of that branch still succeed; the line's head is
/// the rebound session. Resolve it before comparing or rebinding, and do not
/// hold its lease across the rebind.
async fn line_head_locator(
    sessions: &AgentSessions,
    tree_id: &str,
    facts: &ChildFacts,
    locator: SessionLocator,
) -> Result<SessionLocator, TopologyError> {
    let head = sessions
        .line_head_of(locator)
        .await
        .map_err(|error| session_refusal(&error))?;
    if head.session_id() == locator.session_id() && head.branch_id() == locator.branch_id() {
        return Ok(locator);
    }
    let operation_id = recompile_operation_id(tree_id, &facts.agent_id, facts.generation)?;
    let held = sessions
        .open_lease_only(&operation_id, head.line())
        .await
        .map_err(|error| session_refusal(&error))?;
    let current = held.locator();
    held.release()
        .await
        .map_err(|error| session_refusal(&error))?;
    Ok(current)
}

fn recompile_operation_id(
    tree_id: &str,
    agent_id: &str,
    generation: u64,
) -> Result<SessionOperationId, TopologyError> {
    let preferred = format!("plak:child-recompile:{tree_id}:{agent_id}:{generation}");
    if let Ok(id) = SessionOperationId::new(&preferred) {
        return Ok(id);
    }
    let digest = plakous::model::sha256_hex(&[preferred.as_bytes()]);
    SessionOperationId::new(format!("plak:child-recompile:{digest}"))
        .map_err(|_| refused("child recompile operation identity is invalid"))
}
