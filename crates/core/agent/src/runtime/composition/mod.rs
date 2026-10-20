//! Private interpreter ownership; canonical operations remain with Driver.

use crate::local_composition::{
    LocalCompositionDescriptor, LocalCompositionIntegration, LocalCompositionLifecycle,
};
use crate::manifest::InterpreterObservation;
use crate::runtime::OperationId;
use crate::runtime::driver::IndexedToolExecution;
use crate::{AgentError, AgentErrorKind, TelemetryContext, TelemetryEmitter};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use plakous_model::{CallId, Clock, Digest, ToolCorrelation, ToolResultUpdateBlock};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
pub(in crate::runtime) mod resources;
pub(super) use resources::InterpreterResources;

pub(in crate::runtime) mod nested;

/// The actual extracted owner lives in Driver's polled async call. `InFlight`
/// records that cleanup authority must return or be classified as lost.
pub(super) enum CompositionSlot {
    Absent,
    Available(Box<CompositionOwner>),
    InFlight,
}

impl From<Option<CompositionOwner>> for CompositionSlot {
    fn from(owner: Option<CompositionOwner>) -> Self {
        owner.map_or(Self::Absent, |owner| Self::Available(Box::new(owner)))
    }
}

impl CompositionSlot {
    pub(super) fn as_ref(&self) -> Option<&CompositionOwner> {
        match self {
            Self::Available(owner) => Some(owner),
            Self::Absent | Self::InFlight => None,
        }
    }

    pub(super) fn as_mut(&mut self) -> Option<&mut CompositionOwner> {
        match self {
            Self::Available(owner) => Some(owner),
            Self::Absent | Self::InFlight => None,
        }
    }

    pub(super) fn extract(&mut self) -> Result<Box<CompositionOwner>, AgentError> {
        match std::mem::replace(self, Self::InFlight) {
            Self::Available(owner) => Ok(owner),
            other => {
                *self = other;
                Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "local composition owner disappeared",
                ))
            }
        }
    }

    pub(super) fn restore(&mut self, owner: Box<CompositionOwner>) {
        debug_assert!(matches!(self, Self::InFlight));
        *self = Self::Available(owner);
    }

    pub(super) fn is_in_flight(&self) -> bool {
        matches!(self, Self::InFlight)
    }
}

/// What one nested result delivery proved about request ownership.
#[derive(Debug)]
pub(super) enum NestedDeliveryReceipt {
    /// The owning cell settled the exact correlated request.
    Accepted,
    /// The cell refused the acknowledgement because it no longer owns that
    /// request: it was already settled, or was never issued by this cell. The
    /// committed child outcome stands, the cell keeps running, and nothing is
    /// replayed or repeated.
    ///
    /// The refusal and its reason are reported to the operator at the
    /// deployment boundary, by the facade and by the sidecar's own refused
    /// frame. The driver needs only to tell the two receipts apart in order to
    /// keep going, so it carries no detail it would never read.
    NotOwned,
    /// The host proved the cell never observed the result. The cell had
    /// already reached its own definite outcome, so the committed child
    /// outcome stands, nothing is replayed and the cell's own answer is what
    /// the turn reports. Like a refusal, this is a receipt and not a loss.
    NotObserved,
}

pub(super) type NestedDeliveryFuture =
    Pin<Box<dyn Future<Output = Result<NestedDeliveryReceipt, AgentError>> + Send>>;

// Only Driver polls these futures. The mutex permits shared driver borrows
// while the futures need only be Send; polling uses get_mut, never a lock.
#[derive(Default)]
pub(super) struct PendingNestedDeliveries {
    futures: Mutex<FuturesUnordered<NestedDeliveryFuture>>,
    responders: usize,
}

impl PendingNestedDeliveries {
    pub(super) fn len(&self) -> usize {
        self.responders
    }
    pub(super) fn is_empty(&self) -> bool {
        self.responders == 0
    }
    fn futures(&mut self) -> &mut FuturesUnordered<NestedDeliveryFuture> {
        self.futures
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(super) fn push(&mut self, future: NestedDeliveryFuture) {
        self.responders = self.responders.saturating_add(1);
        self.futures().push(future);
    }
    pub(super) fn clear(&mut self) {
        self.futures().clear();
        self.responders = 0;
    }
    pub(super) async fn next(&mut self) -> Option<Result<NestedDeliveryReceipt, AgentError>> {
        let result = self.futures().next().await?;
        self.responders = self.responders.saturating_sub(1);
        Some(result)
    }
}

pub(super) struct CompositionOwner {
    descriptor: LocalCompositionDescriptor,
    integration: Arc<dyn LocalCompositionIntegration>,
    resources: Option<InterpreterResources>,
    cell: Option<String>,
    /// The cell this driver most recently retired.
    ///
    /// The outer marker's own `ToolCompleted` is published after the cell has
    /// been cleared, so `cell` is already `None` there. The lineage contract requires
    /// that the marker carry the cell it owned, and a `wait*`
    /// continuation's terminal is the only place a consumer can learn which
    /// cell it closed, so the retired cell is kept for exactly that
    /// publication. It is dropped when a new cell starts.
    retired_cell: Option<String>,
    source_digest: Option<Digest>,
    nested_sequence: u32,
    child_operations: Vec<OperationId>,
    nested_replays: BTreeMap<String, LocalCompositionNestedReplay>,
    pending_nested: VecDeque<PendingLocalCompositionNested>,
    completed_nested: BTreeMap<u32, LocalCompositionNestedReady>,
    active_nested: usize,
    pending_deliveries: PendingNestedDeliveries,
    next_nested_commit_sequence: u32,
    evaluation_operation: Option<OperationId>,
    exec_call_id: Option<CallId>,
    exec_remote_call_id: Option<String>,
    exec_correlation: Option<ToolCorrelation>,
    pending_updates: Vec<ToolResultUpdateBlock>,
    possible_start: bool,
    started_at: Option<tokio::time::Instant>,
    interpreter_observation: Option<InterpreterObservation>,
}

pub(super) struct PendingLocalCompositionNested {
    pub(super) call: crate::local_composition::LocalCompositionNestedCall,
    pub(super) responder: Box<dyn crate::local_composition::LocalCompositionNestedResponder>,
    pub(super) request_digest: Digest,
    pub(super) request_sequence: u32,
}

pub(super) struct LocalCompositionNestedReadOnlyCompletion {
    pub(super) pending: PendingLocalCompositionNested,
    pub(super) edge_operation: OperationId,
    pub(super) edge_binding: Digest,
    pub(super) evaluation_operation: OperationId,
    pub(super) execution: IndexedToolExecution,
    /// The helper requests reserved before the child was admitted.
    pub(super) helper_reservation: Option<u32>,
}

pub(super) enum LocalCompositionNestedReady {
    Execution(Box<LocalCompositionNestedReadOnlyCompletion>),
    Rejected(Box<(PendingLocalCompositionNested, AgentError)>),
}

pub(super) type LocalCompositionNestedFuture =
    Pin<Box<dyn Future<Output = LocalCompositionNestedReadOnlyCompletion> + Send + 'static>>;

pub(super) struct LocalCompositionNestedReplay {
    pub(super) request_digest: Digest,
    pub(super) request_sequence: u32,
    pub(super) disposition: LocalCompositionNestedDisposition,
    pub(super) waiters: Vec<Box<dyn crate::local_composition::LocalCompositionNestedResponder>>,
}

#[derive(Clone)]
pub(super) enum LocalCompositionNestedDisposition {
    Pending,
    Resolved(serde_json::Value),
    Rejected(String),
    RecoveryRequired,
}

impl CompositionOwner {
    pub(super) fn new(
        construction: super::LocalCompositionConstruction,
        interpreter_observation: Option<InterpreterObservation>,
    ) -> Self {
        Self {
            descriptor: construction.descriptor,
            integration: construction.integration,
            resources: None,
            cell: None,
            retired_cell: None,
            source_digest: None,
            nested_sequence: 0,
            child_operations: Vec::new(),
            nested_replays: BTreeMap::new(),
            pending_nested: VecDeque::new(),
            completed_nested: BTreeMap::new(),
            active_nested: 0,
            pending_deliveries: PendingNestedDeliveries::default(),
            next_nested_commit_sequence: 1,
            evaluation_operation: None,
            exec_call_id: None,
            exec_remote_call_id: None,
            exec_correlation: None,
            pending_updates: Vec::new(),
            possible_start: false,
            started_at: None,
            interpreter_observation,
        }
    }
}

impl CompositionOwner {
    pub(super) fn descriptor(&self) -> &LocalCompositionDescriptor {
        &self.descriptor
    }
    pub(super) fn integration(&self) -> &Arc<dyn LocalCompositionIntegration> {
        &self.integration
    }
    pub(super) fn cell(&self) -> Option<&str> {
        self.cell.as_deref()
    }
    pub(super) fn retired_cell(&self) -> Option<&str> {
        self.retired_cell.as_deref()
    }
    pub(super) fn source_digest(&self) -> Option<Digest> {
        self.source_digest
    }
    pub(super) fn evaluation_operation(&self) -> Option<OperationId> {
        self.evaluation_operation
    }
    pub(super) fn exec_call_id(&self) -> Option<CallId> {
        self.exec_call_id
    }

    pub(super) fn interpreter_observation(&self) -> Option<&InterpreterObservation> {
        self.interpreter_observation.as_ref()
    }
    pub(super) fn started_at(&self) -> Option<tokio::time::Instant> {
        self.started_at
    }
    pub(super) fn take_evaluation(&mut self) -> Option<OperationId> {
        self.evaluation_operation.take()
    }
    pub(super) fn clear_execution_linkage(&mut self) {
        self.exec_call_id = None;
        self.exec_remote_call_id = None;
        self.exec_correlation = None;
    }
    pub(super) fn clear_evaluation(&mut self) {
        self.evaluation_operation = None;
        self.clear_execution_linkage();
    }
    pub(super) fn take_child_operations(&mut self) -> Vec<OperationId> {
        std::mem::take(&mut self.child_operations)
    }
    pub(super) fn record_child_operation(&mut self, operation: OperationId) {
        self.child_operations.push(operation);
    }
    pub(super) fn record_child_operations(&mut self, operations: Vec<OperationId>) {
        self.child_operations.extend(operations);
    }
    pub(super) fn take_updates(&mut self) -> Vec<ToolResultUpdateBlock> {
        std::mem::take(&mut self.pending_updates)
    }
    pub(super) fn push_update(&mut self, update: ToolResultUpdateBlock) {
        self.pending_updates.push(update);
    }
}

impl CompositionOwner {
    pub(super) fn notification_correlation(
        &self,
        cell_id: &str,
        remote_call_id: &str,
    ) -> Result<(CallId, ToolCorrelation), AgentError> {
        if self.cell.as_deref() != Some(cell_id)
            || self.exec_remote_call_id.as_deref() != Some(remote_call_id)
        {
            // A notification is the cell's own printed output: it carries no
            // effect, journals nothing and dispatches nothing. A foreign one is
            // therefore a definite refusal of that one notification, not a
            // turn-ending unknown.
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "local composition notification belongs to a foreign exec call or cell",
            ));
        }
        let canonical_call = self.exec_call_id.ok_or_else(|| {
            local_composition_started_error(
                "local composition notification lost its canonical exec call",
            )
        })?;
        let correlation = self.exec_correlation.clone().ok_or_else(|| {
            local_composition_started_error(
                "local composition notification lost its exec correlation",
            )
        })?;
        Ok((canonical_call, correlation))
    }
}

pub(in crate::runtime) mod record;

pub(in crate::runtime) mod projection;
pub(super) use projection::local_composition_started_error;

#[derive(Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "these independent owner-presence facts make the fail-closed retention predicate explicit"
)]
pub(in crate::runtime) struct LocalCompositionRetentionState {
    pub(in crate::runtime) has_session: bool,
    pub(in crate::runtime) has_events: bool,
    pub(in crate::runtime) has_cell: bool,
    pub(in crate::runtime) possible_start: bool,
    pub(in crate::runtime) has_pending_nested: bool,
    pub(in crate::runtime) has_active_nested: bool,
    pub(in crate::runtime) healthy_idle_observed: bool,
}

pub(in crate::runtime) struct LocalCompositionShutdownTelemetry {
    pub(in crate::runtime) emitter: Arc<TelemetryEmitter>,
    pub(in crate::runtime) clock: Arc<dyn Clock>,
    pub(in crate::runtime) context: TelemetryContext,
}

pub(super) const fn retain_local_composition_at_root_boundary(
    lifecycle: LocalCompositionLifecycle,
    state: LocalCompositionRetentionState,
) -> bool {
    matches!(
        lifecycle,
        LocalCompositionLifecycle::AgentIncarnationPersistent
    ) && state.has_session
        && state.has_events
        && !state.has_cell
        && !state.possible_start
        && !state.has_pending_nested
        && !state.has_active_nested
        && state.healthy_idle_observed
}
