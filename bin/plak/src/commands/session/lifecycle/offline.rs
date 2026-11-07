//! Archive a dormant session while holding the runtime branches against admission.

use plakous::application::ApplicationExecutor;
use plakous::sessions::{
    AgentSessions, AgentSessionsErrorKind, BranchHold, SessionLocator, SessionOperationId,
};
use plakous_hosted::error::CliFailure;
use plakous_recursive_subagents_store::TreeInspection;

use super::{LifecycleError, LifecycleRefusal, SessionActivity};
use crate::shell::services::Services;

struct Fences {
    executor: ApplicationExecutor,
    sessions: std::sync::Arc<AgentSessions>,
    held: Vec<BranchHold>,
}

impl Fences {
    fn acquire(&mut self, recorded: SessionLocator) -> Result<SessionLocator, LifecycleError> {
        let operation = SessionOperationId::new(format!("plak:archive:{}", uuid::Uuid::new_v4()))
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let mut head = recorded;
        for attempt in 0..2 {
            match self
                .executor
                .block_on(self.sessions.hold_branch(&operation, head))
                .map_err(|_| CliFailure::new("application execution failed"))?
            {
                Ok(hold) => {
                    self.held.push(hold);
                    return Ok(head);
                }
                Err(error) if error.kind() == AgentSessionsErrorKind::Ownership => {
                    return Err(LifecycleError::Refused(LifecycleRefusal::Open));
                }
                Err(error)
                    if error.kind() == AgentSessionsErrorKind::StaleLocator && attempt == 0 =>
                {
                    head = error
                        .current_locator()
                        .ok_or_else(|| CliFailure::new("stale runtime head was not named"))?;
                }
                Err(error) => {
                    return Err(
                        CliFailure::new(format!("archive lease fence failed: {error}")).into(),
                    );
                }
            }
        }
        Err(CliFailure::new("runtime head changed while fencing archive").into())
    }

    fn inspect(
        &self,
        head: SessionLocator,
        paths: &crate::home::paths::PlakPaths,
        activity: &mut SessionActivity,
    ) -> Result<(), LifecycleError> {
        let quiescent = self
            .executor
            .block_on(self.sessions.head_is_quiescent(head))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(|error| CliFailure::new(error.to_string()))?;
        activity.turn |= !quiescent;
        if !quiescent {
            return Ok(());
        }
        match AgentSessions::export_history_sqlite(paths.runtime_database(), head) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == AgentSessionsErrorKind::UnsettledProcesses => {
                // The runtime ledger proves unresolved jobs. It does not expose
                // their product process ids here; incomplete evidence is not idle.
                activity.state_unknown = true;
                Ok(())
            }
            Err(error) => {
                Err(CliFailure::new(format!("archive activity read failed: {error}")).into())
            }
        }
    }
}

impl Drop for Fences {
    fn drop(&mut self) {
        for held in self.held.drain(..).rev() {
            let _ = self.executor.block_on(held.release());
        }
    }
}

pub(super) fn archive(services: &Services, id: &str) -> Result<(), LifecycleError> {
    let paths = services.paths()?;
    let product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if product.lifecycle_metadata(id)?.1 {
        return Ok(());
    }
    let records = services.session_records()?;
    let record = records.load(id).map_err(CliFailure::new)?;
    let mut fences = Fences {
        executor: ApplicationExecutor::new()
            .map_err(|_| CliFailure::new("application executor unavailable"))?,
        sessions: services.state_roots()?.runtime_sessions()?,
        held: Vec::new(),
    };
    let mut activity = SessionActivity {
        turn: false,
        children: Vec::new(),
        processes: Vec::new(),
        state_unknown: false,
    };
    let Some(locator) = record.plakous_locator else {
        // No existing branch can fence a concurrent first runtime creation.
        // The host can serialize that reservation; offline cannot prove idle.
        activity.state_unknown = true;
        return super::set_archived(services, id, true, &activity);
    };
    let head = fences.acquire(locator)?;
    fences.inspect(head, paths, &mut activity)?;
    let (_, inspected) = crate::topology_product::inspect_session_tree(paths, &record)?;
    if let TreeInspection::Tree(tree) = inspected {
        for node in tree
            .rows()
            .nodes()
            .values()
            .filter(|node| node.parent_agent_id.is_some())
        {
            if !node.status.is_finished() {
                activity.children.push(node.agent_id.clone());
                continue;
            }
            if let Some(locator) = &node.session_locator {
                let locator = locator
                    .parse::<SessionLocator>()
                    .map_err(|error| CliFailure::new(error.to_string()))?;
                let head = fences.acquire(locator)?;
                let mut child_activity = SessionActivity {
                    turn: false,
                    children: Vec::new(),
                    processes: Vec::new(),
                    state_unknown: false,
                };
                fences.inspect(head, paths, &mut child_activity)?;
                if child_activity.turn {
                    activity.children.push(node.agent_id.clone());
                }
                activity.state_unknown |= child_activity.state_unknown;
            }
        }
    }
    super::set_archived(services, id, true, &activity)
}
