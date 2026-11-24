//! Read-only backend for exact runtime construction probes.

use super::*;
use plakous_agent::internal::{
    LineId, LineJobTerminal, LineRecord, LineTerminalsReceipt, RecordLineJobTerminals,
};
use std::path::PathBuf;

struct ReopenInspection(PathBuf);

impl SqliteSessionStore {
    /// Supplies read-only storage for exact runtime verification. No writer,
    /// actor, holder or clock authority is opened.
    #[doc(hidden)]
    #[must_use]
    pub fn reopen_inspection(path: PathBuf) -> impl SessionStore {
        ReopenInspection(path)
    }
}

impl ReopenInspection {
    fn snapshot<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, SessionStoreError>,
    ) -> Result<T, SessionStoreError> {
        let connection =
            SqliteSessionStore::open_inspection(&self.0)?.ok_or(SessionStoreError::NotFound)?;
        let snapshot = connection.unchecked_transaction().map_err(map_sqlite)?;
        admit_inspection_snapshot(&snapshot)?;
        read(&snapshot)
    }
}

// All methods outside the verification read set fail closed. In particular,
// an inspection backend cannot grant a lease or publish even by mistake.
macro_rules! refuse {
    ($($name:ident($($arg:ident: $input:ty),*) -> $output:ty;)*) => {
        $(
            fn $name(&self $(, $arg: $input)*) -> StoreFuture<'_, $output> {
                Box::pin(async { Err(SessionStoreError::BackendUnavailable) })
            }
        )*
    };
}

impl SessionStore for ReopenInspection {
    fn load_branch(
        &self,
        session: SessionRef,
        _operation: &'static str,
    ) -> StoreFuture<'_, LoadedBranch> {
        Box::pin(async move { self.snapshot(|c| load_branch(c, session)) })
    }

    fn load_checkpoint(&self, checkpoint: CheckpointId) -> StoreFuture<'_, StoredCheckpoint> {
        Box::pin(async move { self.snapshot(|c| load_checkpoint(c, checkpoint)) })
    }

    fn branch_line(&self, session: SessionRef) -> StoreFuture<'_, LineId> {
        Box::pin(async move {
            self.snapshot(|c| {
                lines::branch_line(c, session)?
                    .map(|(line, _)| line)
                    .ok_or(SessionStoreError::NotFound)
            })
        })
    }

    fn load_line(&self, line: LineId) -> StoreFuture<'_, LineRecord> {
        Box::pin(async move { self.snapshot(|c| lines::load_line(c, line)) })
    }

    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        Err(SessionStoreError::BackendUnavailable)
    }

    refuse! {
        create(_request: CreateSession) -> CreateReceipt;
        recover_create(_request: CreateSession) -> CreateReceipt;
        acquire(_request: AcquireBranch) -> BranchLease;
        renew(_lease: BranchLease) -> BranchLease;
        release(_lease: BranchLease) -> ();
        commit(_request: CommitCheckpoint) -> CommitReceipt;
        recover_commit(_checkpoint: CheckpointId) -> CommittedCheckpoint;
        fork(_request: ForkBranch) -> ForkReceipt;
        recover_fork(_request: RecoverFork) -> ForkReceipt;
        list(_query: SessionQuery) -> Page<SessionSummary>;
        usage() -> StoreUsage;
        archive(_request: ArchiveSession) -> ArchiveReceipt;
        delete(_request: DeleteSession) -> DeleteReceipt;
        recover_delete(_request: DeleteSession) -> DeleteReceipt;
        read_feed(_session_id: SessionId, _after: FeedSeq, _max_records: NonZeroUsize) -> FeedPage;
        feed_tail(_session_id: SessionId) -> FeedTail;
        line_job_terminals(_line: LineId) -> Vec<LineJobTerminal>;
        record_line_job_terminals(_request: RecordLineJobTerminals) -> LineTerminalsReceipt;
        flush() -> DurabilityReport;
        shutdown() -> DurabilityReport;
    }
}
