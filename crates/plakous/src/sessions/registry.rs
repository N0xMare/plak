use super::{AgentSessionsError, AgentSessionsErrorKind, SessionOperationId};
use plakous_agent::internal::facade_support::{
    AgentSessionEndpoint, WeakAgentSessionEndpoint, agent_session_endpoint,
};
use plakous_agent::{Agent, AgentId};
use plakous_model::{BranchId, SessionId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct SessionBranchKey {
    session_id: SessionId,
    branch_id: BranchId,
}

impl SessionBranchKey {
    pub(super) fn new(session_id: SessionId, branch_id: BranchId) -> Self {
        Self {
            session_id,
            branch_id,
        }
    }

    fn from_agent(agent: &Agent) -> Self {
        Self {
            session_id: agent.session_id(),
            branch_id: agent.branch_id(),
        }
    }
}

#[derive(Clone)]
struct ActiveDriver {
    agent_id: AgentId,
    endpoint: WeakAgentSessionEndpoint,
}

pub(super) struct DriverRegistration {
    operation_id: SessionOperationId,
    branch: SessionBranchKey,
    agent_id: AgentId,
    registry: Arc<DriverRegistry>,
}

impl Drop for DriverRegistration {
    fn drop(&mut self) {
        self.registry.unregister(self);
    }
}

pub(super) struct DriverReservation {
    operation_id: SessionOperationId,
    registry: Arc<DriverRegistry>,
}

pub(super) struct SessionLifecycleReservation {
    session_id: SessionId,
    registry: Arc<DriverRegistry>,
}

impl Drop for SessionLifecycleReservation {
    fn drop(&mut self) {
        self.registry
            .lock()
            .destructive_sessions
            .remove(&self.session_id);
    }
}

impl Drop for DriverReservation {
    fn drop(&mut self) {
        self.registry.abandon(&self.operation_id);
    }
}

#[derive(Default)]
struct DriverRegistryState {
    reserved: BTreeSet<SessionOperationId>,
    destructive_sessions: BTreeSet<SessionId>,
    by_operation: BTreeMap<SessionOperationId, ActiveDriver>,
    by_branch: BTreeMap<SessionBranchKey, ActiveDriver>,
}

#[derive(Default)]
pub(super) struct DriverRegistry {
    state: Mutex<DriverRegistryState>,
}

impl DriverRegistry {
    pub(super) fn reserve_session(
        self: &Arc<Self>,
        session_id: SessionId,
    ) -> Result<SessionLifecycleReservation, AgentSessionsError> {
        let mut state = self.lock();
        Self::prune(&mut state);
        let attached = state
            .by_branch
            .keys()
            .any(|key| key.session_id == session_id);
        if attached || !state.destructive_sessions.insert(session_id) {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::Ownership,
                "session has an attached or destructive private workflow",
            ));
        }
        Ok(SessionLifecycleReservation {
            session_id,
            registry: Arc::clone(self),
        })
    }

    pub(super) fn reserve(
        self: &Arc<Self>,
        operation_id: &SessionOperationId,
    ) -> Result<DriverReservation, AgentSessionsError> {
        let mut state = self.lock();
        Self::prune(&mut state);
        if state.reserved.contains(operation_id) || state.by_operation.contains_key(operation_id) {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::Ownership,
                "session operation already has an active private driver",
            ));
        }
        state.reserved.insert(operation_id.clone());
        Ok(DriverReservation {
            operation_id: operation_id.clone(),
            registry: Arc::clone(self),
        })
    }

    fn abandon(&self, operation_id: &SessionOperationId) {
        self.lock().reserved.remove(operation_id);
    }

    pub(super) fn register(
        self: &Arc<Self>,
        operation_id: SessionOperationId,
        agent: &Agent,
    ) -> Result<DriverRegistration, AgentSessionsError> {
        let mut state = self.lock();
        Self::prune(&mut state);
        state.reserved.remove(&operation_id);
        let branch = SessionBranchKey::from_agent(agent);
        if state.by_branch.contains_key(&branch)
            || state.destructive_sessions.contains(&branch.session_id)
        {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::Ownership,
                "session branch already has an active private driver",
            ));
        }
        let active = ActiveDriver {
            agent_id: agent.id(),
            endpoint: agent_session_endpoint(agent).downgrade(),
        };
        state
            .by_operation
            .insert(operation_id.clone(), active.clone());
        state.by_branch.insert(branch, active);
        Ok(DriverRegistration {
            operation_id,
            branch,
            agent_id: agent.id(),
            registry: Arc::clone(self),
        })
    }

    pub(super) fn unregister(&self, registration: &DriverRegistration) {
        let mut state = self.lock();
        if state
            .by_operation
            .get(&registration.operation_id)
            .is_some_and(|active| active.agent_id == registration.agent_id)
        {
            state.by_operation.remove(&registration.operation_id);
        }
        if state
            .by_branch
            .get(&registration.branch)
            .is_some_and(|active| active.agent_id == registration.agent_id)
        {
            state.by_branch.remove(&registration.branch);
        }
    }

    pub(super) fn endpoint(&self, branch: SessionBranchKey) -> Option<AgentSessionEndpoint> {
        let mut state = self.lock();
        Self::prune(&mut state);
        let endpoint = state
            .by_branch
            .get(&branch)
            .and_then(|active| active.endpoint.upgrade());
        drop(state);
        endpoint
    }

    fn lock(&self) -> MutexGuard<'_, DriverRegistryState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn prune(state: &mut DriverRegistryState) {
        state
            .by_operation
            .retain(|_, active| active.endpoint.upgrade().is_some());
        state
            .by_branch
            .retain(|_, active| active.endpoint.upgrade().is_some());
    }
}
