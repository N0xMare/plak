use super::{
    AbandonCodeCellRequest, AbandonCompactionRequest, Digest, RecoverEffectEvidenceRequest,
    RecoverSessionRequest, RecoveryObligationReport, RecoveryRequiredReport, SessionEffectEvidence,
    SessionLocator, SessionOperationId, SessionRecoveryAction, SessionRecoveryPhase,
    SessionRuntimeBinding, ToolExecutorId, ToolOutcome, fmt,
};

impl RecoverSessionRequest {
    /// Creates an exact recovery request.
    #[must_use]
    pub const fn new(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            runtime,
        }
    }
}

impl AbandonCodeCellRequest {
    /// Binds one retry-stable abandonment operation to the exact working root.
    #[must_use]
    pub const fn new(operation_id: SessionOperationId, locator: SessionLocator) -> Self {
        Self {
            operation_id,
            locator,
        }
    }
}

impl AbandonCompactionRequest {
    /// Binds abandonment to the original compaction identity and source.
    #[must_use]
    pub const fn new(operation_id: SessionOperationId, locator: SessionLocator) -> Self {
        Self {
            operation_id,
            locator,
        }
    }
}

impl SessionEffectEvidence {
    /// Prepared effect that has not been claimed or dispatched.
    #[must_use]
    pub const fn prepared() -> Self {
        Self {
            claim_digest: None,
            dispatched: false,
        }
    }

    /// Claimed mutating effect with the exact persisted claim-evidence digest.
    #[must_use]
    pub const fn claimed(claim_digest: Digest) -> Self {
        Self {
            claim_digest: Some(claim_digest),
            dispatched: false,
        }
    }

    /// Dispatched effect, optionally bound to the prior claim digest.
    #[must_use]
    pub const fn dispatched(claim_digest: Option<Digest>) -> Self {
        Self {
            claim_digest,
            dispatched: true,
        }
    }

    /// Returns the persisted claim-evidence digest, if any.
    #[must_use]
    pub const fn claim_digest(self) -> Option<Digest> {
        self.claim_digest
    }

    /// Reports whether physical dispatch was observed for this evidence.
    #[must_use]
    pub const fn was_dispatched(self) -> bool {
        self.dispatched
    }
}

impl RecoverEffectEvidenceRequest {
    /// Binds one retry-stable workflow to an exact Working effect obligation.
    #[must_use]
    pub const fn new(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        recovery_operation: u64,
        evidence: SessionEffectEvidence,
        executor: ToolExecutorId,
        outcome: ToolOutcome,
    ) -> Self {
        Self {
            operation_id,
            locator,
            recovery_operation,
            evidence,
            executor,
            outcome,
        }
    }
}

impl fmt::Debug for RecoverEffectEvidenceRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoverEffectEvidenceRequest")
            .field("operation_id", &self.operation_id)
            .field("locator", &self.locator)
            .field("recovery_operation", &self.recovery_operation)
            .field("evidence", &self.evidence)
            .field("executor", &self.executor)
            .field("outcome", &self.outcome)
            .finish()
    }
}

impl RecoveryObligationReport {
    /// Returns the persisted operation identity.
    #[must_use]
    pub const fn operation_id(self) -> u64 {
        self.operation_id
    }

    /// Returns the only recovery action authorized by the checkpoint.
    #[must_use]
    pub const fn action(self) -> SessionRecoveryAction {
        self.action
    }

    /// Returns the exact durable request identity to which evidence must bind.
    #[must_use]
    pub const fn request_identity(self) -> Digest {
        self.request_identity
    }
}

impl RecoveryRequiredReport {
    /// Returns the exact working checkpoint.
    #[must_use]
    pub const fn locator(&self) -> SessionLocator {
        self.locator
    }
    /// Returns the exact recorded recovery phase.
    #[must_use]
    pub const fn phase(&self) -> SessionRecoveryPhase {
        self.phase
    }
    /// Returns every exact unresolved operation.
    #[must_use]
    pub fn obligations(&self) -> &[RecoveryObligationReport] {
        &self.obligations
    }
}
