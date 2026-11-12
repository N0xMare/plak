//! Private host terminal authority; no client wire vocabulary or agent records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use plakous::runner::RunnerClient;
use plakous::runner::protocol::operator::{
    OperatorAnswer, OperatorId, OperatorNamespace, OperatorOperation, OperatorRefusal, TerminalKey,
    TerminalRow, TerminalStatus,
};
use plakous_hosted::host_service::ResidencyHold;
use plakous_hosted::host_service::wire::{HostPrincipal, HostRequestContext};
use plakous_hosted::workspace_reclamation::{ReclamationAdmission, ReclamationFence, VcsError};

use crate::home::paths::PlakPaths;
use crate::store::ProductStore;

mod control;
pub(crate) use control::TerminalController;
mod lifecycle;
mod pins;
mod placement;
mod reclamation;
mod recovery;
use placement::TerminalPin;
pub(crate) use reclamation::{RunnerFence, session_pinned};
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "isolated host terminal authority fixtures"
)]
mod tests;

type Hold = dyn Fn(&str) -> Result<ResidencyHold, String> + Send + Sync;

pub(crate) struct TerminalRegistry {
    paths: PlakPaths,
    generation: u64,
    runner_generation: String,
    host_id: String,
    state_key: OperatorId,
    state: Arc<Mutex<State>>,
    operations: tokio::sync::Mutex<()>,
    hold: std::sync::OnceLock<Arc<Hold>>,
}

#[derive(Default)]
struct State {
    residents: BTreeMap<String, plakous_hosted::host_service::AttachmentCount>,
    closing: bool,
    entries: BTreeMap<String, Entry>,
    bindings: BTreeMap<String, Binding>,
    reclaiming: BTreeMap<(String, PathBuf), usize>,
    controllers: BTreeMap<String, ControllerOwner>,
}

struct Entry {
    pin: TerminalPin,
    pinned: bool,
    lost: bool,
    row: Option<TerminalRow>,
    hold: Option<ResidencyHold>,
}

#[derive(Clone)]
struct Binding {
    client: RunnerClient,
    revision: Arc<plakous::runner::RunnerConnectionRevision>,
    reconciled: bool,
}

struct ControllerOwner {
    token: String,
    principal: HostPrincipal,
    connection: u64,
    cancel: tokio::sync::watch::Sender<bool>,
}

/// Becomes a client-9 result only with that increment's explicit DTO mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResidentReason {
    OperatorTerminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalError {
    Lost,
    Busy,
    Unavailable,
    OutcomeUnknown,
    Storage,
    Conflict,
    Refused(OperatorRefusal),
}

impl TerminalRegistry {
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn new(paths: PlakPaths, generation: u64) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            state_key: OperatorId::try_from(crate::home::state::state_key())
                .map_err(str::to_owned)?,
            host_id: crate::commands::runner::host_id(&paths)
                .map_err(|_| "terminal host identity unavailable")?,
            runner_generation: plakous_recursive_subagents::tree::process_instance().to_string(),
            paths,
            generation,
            state: Arc::default(),
            operations: tokio::sync::Mutex::new(()),
            hold: std::sync::OnceLock::new(),
        }))
    }

    pub(crate) fn bind_residency(&self, hold: Arc<Hold>) -> Result<(), String> {
        self.hold
            .set(hold)
            .map_err(|_| "terminal residency already bound".into())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn namespace(&self, principal: &HostPrincipal) -> Result<OperatorNamespace, TerminalError> {
        let principal = match principal {
            HostPrincipal::Socket(uid) => format!("socket:{uid}"),
            HostPrincipal::Web(login) => {
                format!(
                    "web:{}",
                    plakous_hosted::workspace_maintenance::digest_text(login)
                )
            }
        };
        Ok(OperatorNamespace {
            state_key: self.state_key.clone(),
            principal: id(principal)?,
        })
    }

    fn authenticate(&self, context: &HostRequestContext) -> Result<(), TerminalError> {
        if context.host_generation != self.generation || self.lock().closing {
            return Err(TerminalError::Lost);
        }
        Ok(())
    }

    fn binding(&self, runner: &str) -> Result<Binding, TerminalError> {
        let binding = self
            .lock()
            .bindings
            .get(runner)
            .cloned()
            .ok_or(TerminalError::Unavailable)?;
        if !binding.reconciled || !binding.revision.is_current() {
            return Err(TerminalError::Unavailable);
        }
        Ok(binding)
    }

    async fn request(
        binding: &Binding,
        namespace: OperatorNamespace,
        operation: OperatorOperation,
    ) -> Result<OperatorAnswer, TerminalError> {
        if !binding.revision.is_current() {
            return Err(TerminalError::Unavailable);
        }
        let answer = binding
            .client
            .operator_control(namespace, operation)
            .await
            .map_err(|failure| match failure {
                plakous::tools::runner::RunnerFailure::OutcomeUnknown(_) => {
                    TerminalError::OutcomeUnknown
                }
                _ => TerminalError::Unavailable,
            })?;
        if !binding.revision.is_current() {
            return Err(TerminalError::OutcomeUnknown);
        }
        match answer {
            OperatorAnswer::Refused { refusal } => Err(TerminalError::Refused(refusal)),
            answer => Ok(answer),
        }
    }

    pub(crate) fn resident_reason(&self, session: &str) -> Option<ResidentReason> {
        self.lock()
            .entries
            .values()
            .any(|entry| entry.pinned && entry.pin.key.session.as_str() == session)
            .then_some(ResidentReason::OperatorTerminal)
    }

    pub(crate) fn blocks_idle_exit(&self) -> bool {
        self.lock().entries.values().any(|entry| entry.pinned)
    }

    pub(crate) fn disconnect(&self, principal: &HostPrincipal, connection: u64, generation: u64) {
        if generation != self.generation {
            return;
        }
        self.lock().controllers.retain(|_, owner| {
            let remove = &owner.principal == principal && owner.connection == connection;
            if remove {
                let _ = owner.cancel.send(true);
            }
            !remove
        });
    }

    fn store(&self) -> Result<ProductStore, TerminalError> {
        ProductStore::open(&self.paths).map_err(|_| TerminalError::Storage)
    }

    fn release_proven(&self, identity: &str) -> Result<(), TerminalError> {
        let mut state = self.lock();
        let entry = state.entries.get_mut(identity).ok_or(TerminalError::Lost)?;
        if !entry.pinned {
            return Ok(());
        }
        entry.pinned = false;
        entry.hold = None;
        Ok(())
    }

    fn owned(
        &self,
        context: &HostRequestContext,
        handle: &TerminalHandle,
    ) -> Result<TerminalPin, TerminalError> {
        self.authenticate(context)?;
        let namespace = self.namespace(&context.principal)?;
        let state = self.lock();
        let entry = state
            .entries
            .get(&handle.identity)
            .ok_or(TerminalError::Lost)?;
        if entry.pin.namespace != namespace
            || entry.pin.host_id != self.host_id
            || handle.instance != entry.pin.instance
        {
            return Err(TerminalError::Lost);
        }
        Ok(entry.pin.clone())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalHandle {
    pub(crate) identity: String,
    pub(crate) host_generation: u64,
    pub(crate) instance: String,
}

pub(crate) struct TerminalView {
    pub handle: TerminalHandle,
    pub key: TerminalKey,
    pub row: Option<TerminalRow>,
    pub lost: bool,
}

fn id(value: String) -> Result<OperatorId, TerminalError> {
    OperatorId::try_from(value).map_err(|_| TerminalError::Conflict)
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
