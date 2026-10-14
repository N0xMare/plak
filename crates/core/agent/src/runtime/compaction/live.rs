use crate::runtime::compaction::{
    CompactOrigin, DurableCompactionIdentity, DurableCompactionRequestRecord,
    ExactCompactionRuntime,
};
use crate::runtime::driver::model_cleanup::ambiguous_compaction_error;
use crate::runtime::durable::DurableSessionAddress;
use crate::runtime::{OperationId, PromptCommand};
use crate::{AgentError, AgentErrorKind};
use plakous_model::{
    CancellationToken, ModelCallContext, ModelError, ModelEvent, ModelResponse, PreparedCallRecord,
    PreparedModelCall, ProviderCallRecoveryLocator, ProviderCallRecoveryLocatorStage, TurnId,
    ValidatedRequestPlan,
};
use tokio::time::Instant;
/// Canonical publication remains with Driver. This owner retains only the
/// one-shot provider capability, fixed call budget and observed stream evidence.
pub(in super::super) struct LiveCompaction {
    origin: CompactOrigin,
    active: Option<PromptCommand>,
    cancellation: CancellationToken,
    identity: DurableCompactionIdentity,
    operations: (OperationId, OperationId),
    record: DurableCompactionRequestRecord,
    prepared_record: PreparedCallRecord,
    plan: ValidatedRequestPlan,
    runtime: ExactCompactionRuntime,
    deadline: Option<Instant>,
    learned_locator: Option<ProviderCallRecoveryLocator>,
    phase: Phase,
}

enum Phase {
    Prepared(Box<PreparedModelCall>),
    Started {
        completed: Option<Box<ModelResponse>>,
    },
    Drained,
}

pub(in super::super) struct ValidatedCompactionLocator {
    locator: ProviderCallRecoveryLocator,
}

impl ValidatedCompactionLocator {
    pub(in super::super) fn locator(&self) -> &ProviderCallRecoveryLocator {
        &self.locator
    }
}

pub(in super::super) enum CompactionStreamError {
    ContradictoryTerminal,
    ForeignPlan,
    InvalidTerminal(ModelError),
}

impl LiveCompaction {
    #[allow(
        clippy::too_many_arguments,
        reason = "one exact compaction operation carries its immutable authority and sole prepared capability"
    )]
    pub(in super::super) fn prepared(
        origin: CompactOrigin,
        active: Option<PromptCommand>,
        identity: DurableCompactionIdentity,
        operations: (OperationId, OperationId),
        record: DurableCompactionRequestRecord,
        prepared: PreparedModelCall,
        plan: ValidatedRequestPlan,
        runtime: ExactCompactionRuntime,
    ) -> Self {
        let cancellation = active
            .as_ref()
            .map_or_else(CancellationToken::new, |command| {
                command.cancellation.clone()
            });
        Self {
            origin,
            active,
            cancellation,
            identity,
            operations,
            record,
            prepared_record: prepared.record().clone(),
            plan,
            runtime,
            deadline: None,
            learned_locator: None,
            phase: Phase::Prepared(Box::new(prepared)),
        }
    }

    /// Consumed synchronously by Driver under its final lease admission guard.
    pub(in super::super) fn take_prepared(&mut self) -> Result<PreparedModelCall, AgentError> {
        if self.deadline.is_none() {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "compaction call budget was not started",
            ));
        }
        match std::mem::replace(&mut self.phase, Phase::Drained) {
            Phase::Prepared(prepared) => {
                self.phase = Phase::Started { completed: None };
                Ok(*prepared)
            }
            phase => {
                self.phase = phase;
                Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "compaction prepared capability was already consumed",
                ))
            }
        }
    }

    pub(in super::super) fn validate_locator(
        &self,
        locator: ProviderCallRecoveryLocator,
    ) -> Result<ValidatedCompactionLocator, AgentError> {
        if !matches!(self.phase, Phase::Started { .. })
            || self.learned_locator.is_some()
            || locator.availability_stage()
                != ProviderCallRecoveryLocatorStage::LearnedAfterDispatch
            || self
                .prepared_record
                .recovery_request_with_locator(locator.clone())
                .is_err()
        {
            return Err(ambiguous_compaction_error(
                "provider produced contradictory compaction recovery evidence",
            ));
        }
        Ok(ValidatedCompactionLocator { locator })
    }

    /// Only called after Driver has durably published this exact locator.
    pub(in super::super) fn locator_published(&mut self, locator: ValidatedCompactionLocator) {
        self.learned_locator = Some(locator.locator);
    }

    pub(in super::super) fn observe_event(
        &mut self,
        event: ModelEvent,
    ) -> Result<(), CompactionStreamError> {
        let Phase::Started { completed } = &mut self.phase else {
            return Err(CompactionStreamError::ContradictoryTerminal);
        };
        if completed.is_some() {
            return Err(CompactionStreamError::ContradictoryTerminal);
        }
        if let ModelEvent::ResponseCompleted(response) = event {
            if response.plan_digest() != self.plan.digest() {
                return Err(CompactionStreamError::ForeignPlan);
            }
            response
                .validate_for_commit()
                .map_err(CompactionStreamError::InvalidTerminal)?;
            *completed = Some(Box::new(response));
        }
        Ok(())
    }

    pub(in super::super) fn finish_stream(&mut self) -> Result<ModelResponse, AgentError> {
        match std::mem::replace(&mut self.phase, Phase::Drained) {
            Phase::Started {
                completed: Some(response),
            } => Ok(*response),
            phase => {
                self.phase = phase;
                Err(ambiguous_compaction_error(
                    "compaction stream ended without terminal provider evidence",
                ))
            }
        }
    }

    /// Seal the call budget at Driver's existing run-entry boundary, before
    /// waiting on provider or lease admission. Later polls cannot restart it.
    pub(in super::super) fn begin_call(
        &mut self,
    ) -> Result<(Instant, ModelCallContext), AgentError> {
        if self.deadline.is_some() {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "compaction call budget was already started",
            ));
        }
        let started = Instant::now();
        let at = started + self.runtime.model_operation_timeout;
        let deadline = self.deadline.insert(at);
        Ok((
            *deadline,
            ModelCallContext::new(self.identity.attempt_id, self.cancellation.clone(), None)
                .with_dispatch_timeout(self.runtime.model_operation_timeout),
        ))
    }
    pub(in super::super) fn active(&self) -> Option<&PromptCommand> {
        self.active.as_ref()
    }
    pub(in super::super) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
    pub(in super::super) fn identity(&self) -> DurableCompactionIdentity {
        self.identity
    }
    pub(in super::super) fn operations(&self) -> (OperationId, OperationId) {
        self.operations
    }
    pub(in super::super) fn prepared_record(&self) -> &PreparedCallRecord {
        &self.prepared_record
    }
    pub(in super::super) fn plan(&self) -> &ValidatedRequestPlan {
        &self.plan
    }
    pub(in super::super) fn runtime(&self) -> &ExactCompactionRuntime {
        &self.runtime
    }
    pub(in super::super) fn record(&self) -> &DurableCompactionRequestRecord {
        &self.record
    }
    pub(in super::super) fn origin(&self) -> CompactOrigin {
        self.origin
    }
    pub(in super::super) fn source(&self) -> DurableSessionAddress {
        self.identity.source
    }
    pub(in super::super) fn turn_id(&self) -> Option<TurnId> {
        self.active.as_ref().map(|command| command.turn_id)
    }
}
