use super::{
    ArchiveSessionReport, ArchivedSessionReport, CreateSessionRequest, DeleteSessionReport,
    OpenSessionRequest, ReclaimReport, SessionId, SessionLocator, SessionOperationId,
    SessionRuntimeBinding,
};

impl CreateSessionRequest {
    /// Creates an exact caller-identified request.
    #[must_use]
    pub const fn new(operation_id: SessionOperationId, runtime: SessionRuntimeBinding) -> Self {
        Self {
            operation_id,
            runtime,
            history: None,
            line: None,
        }
    }
}

impl OpenSessionRequest {
    /// Creates an exact caller-identified open request.
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

    /// Makes the open close a turn the previous owner left waiting at an
    /// effect decision instead of restoring it there: the turn ends abandoned
    /// with `cause`, the decision never made and its effect never started
    /// (spec 01 §5.5). For a new owner that does not take over the previous
    /// owner's pending decisions. A promoted server pause (its response
    /// already in history) closes the same way, after its model round; a
    /// tool-batch frame, or a pause never promoted, restores.
    ///
    /// The caller asserts `cause`: `ProcessLost` only when the previous
    /// owner was lost, `Interrupted` when it let go of the turn (a planned
    /// handoff). Best effort: a turn the open could not otherwise abandon is
    /// restored as if this were not set.
    #[must_use]
    pub fn closing_effect_decision_barriers(
        mut self,
        cause: plakous_agent::AbandonmentCause,
    ) -> Self {
        self.runtime.builder = self.runtime.builder.closing_effect_decision_barriers(cause);
        self
    }
}

impl ArchiveSessionReport {
    /// Returns the archived session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }
    /// Returns the exact pre-archive revision accepted by the store.
    #[must_use]
    pub const fn previous_revision(self) -> u64 {
        self.previous_revision
    }
    /// Returns the exact archived revision required by deletion.
    #[must_use]
    pub const fn archived_revision(self) -> u64 {
        self.archived_revision
    }
}

impl ArchivedSessionReport {
    /// Returns the archived session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }
    /// Returns the exact archived revision deletion requires.
    #[must_use]
    pub const fn archived_revision(self) -> u64 {
        self.archived_revision
    }
}

impl DeleteSessionReport {
    /// Returns the deleted session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }
    /// Returns the exact archived revision that authorized deletion.
    #[must_use]
    pub const fn archived_revision(self) -> u64 {
        self.archived_revision
    }
    /// Returns removed checkpoint roots.
    #[must_use]
    pub const fn reclaimed_checkpoints(self) -> u64 {
        self.reclaimed_checkpoints
    }
}

impl ReclaimReport {
    /// Returns deleted content-addressed objects.
    #[must_use]
    pub const fn objects(self) -> u64 {
        self.objects
    }
    /// Returns deleted payload and edge bytes.
    #[must_use]
    pub const fn object_bytes(self) -> u64 {
        self.object_bytes
    }
}
