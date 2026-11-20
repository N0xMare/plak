/// Implements the named `SessionStore` methods for a test double. Other
/// methods are left to the wrapper or to the trait's defaults.
#[macro_export]
macro_rules! session_store_methods {
    (forward $api:ident, $inner:ident; $($method:ident),+ $(,)?) => {
        $($crate::session_store_methods!(@method [$api forward $inner] $method);)+
    };
    (unavailable $api:ident; $($method:ident),+ $(,)?) => {
        $($crate::session_store_methods!(@method [$api unavailable] $method);)+
    };

    (@method [$api:ident forward $inner:ident] lease_now) => {
        fn lease_now(&self) -> Result<$api::LeaseInstant, $api::SessionStoreError> {
            self.$inner.lease_now()
        }
    };
    (@method [$api:ident $($target:tt)+] create) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            create(request: $api::CreateSession) -> $api::CreateReceipt);
    };
    (@method [$api:ident $($target:tt)+] recover_create) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            recover_create(request: $api::CreateSession) -> $api::CreateReceipt);
    };
    (@method [$api:ident $($target:tt)+] load_branch) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            load_branch(session: $api::SessionRef) -> $api::LoadedBranch);
    };
    (@method [$api:ident $($target:tt)+] load_checkpoint) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            load_checkpoint(checkpoint: ::plakous_model::CheckpointId) -> $api::StoredCheckpoint);
    };
    (@method [$api:ident $($target:tt)+] acquire) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            acquire(request: $api::AcquireBranch) -> $api::BranchLease);
    };
    (@method [$api:ident $($target:tt)+] renew) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            renew(lease: $api::BranchLease) -> $api::BranchLease);
    };
    (@method [$api:ident $($target:tt)+] release) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            release(lease: $api::BranchLease) -> ());
    };
    (@method [$api:ident $($target:tt)+] commit) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            commit(request: $api::CommitCheckpoint) -> $api::CommitReceipt);
    };
    (@method [$api:ident $($target:tt)+] recover_commit) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            recover_commit(checkpoint: ::plakous_model::CheckpointId) -> $api::CommittedCheckpoint);
    };
    (@method [$api:ident $($target:tt)+] fork) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            fork(request: $api::ForkBranch) -> $api::ForkReceipt);
    };
    (@method [$api:ident $($target:tt)+] recover_fork) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            recover_fork(request: $api::RecoverFork) -> $api::ForkReceipt);
    };
    (@method [$api:ident $($target:tt)+] list) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            list(query: $api::SessionQuery) -> $api::Page<$api::SessionSummary>);
    };
    (@method [$api:ident $($target:tt)+] usage) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            usage() -> $api::StoreUsage);
    };
    (@method [$api:ident $($target:tt)+] archive) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            archive(request: $api::ArchiveSession) -> $api::ArchiveReceipt);
    };
    (@method [$api:ident $($target:tt)+] delete) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            delete(request: $api::DeleteSession) -> $api::DeleteReceipt);
    };
    (@method [$api:ident $($target:tt)+] prune) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            prune(request: $api::PruneBranch) -> $api::PruneReceipt);
    };
    (@method [$api:ident $($target:tt)+] recover_delete) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            recover_delete(request: $api::DeleteSession) -> $api::DeleteReceipt);
    };
    (@method [$api:ident $($target:tt)+] read_feed) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            read_feed(session: ::plakous_model::SessionId, after: $api::FeedSeq, max: ::std::num::NonZeroUsize) -> $api::FeedPage);
    };
    (@method [$api:ident $($target:tt)+] feed_tail) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            feed_tail(session: ::plakous_model::SessionId) -> $api::FeedTail);
    };
    (@method [$api:ident $($target:tt)+] branch_line) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            branch_line(session: $api::SessionRef) -> ::plakous_model::LineId);
    };
    (@method [$api:ident $($target:tt)+] load_line) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            load_line(line: ::plakous_model::LineId) -> $api::LineRecord);
    };
    (@method [$api:ident $($target:tt)+] line_job_terminals) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            line_job_terminals(line: ::plakous_model::LineId) -> Vec<$api::LineJobTerminal>);
    };
    (@method [$api:ident $($target:tt)+] record_line_job_terminals) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            record_line_job_terminals(request: $api::RecordLineJobTerminals) -> $api::LineTerminalsReceipt);
    };
    (@method [$api:ident $($target:tt)+] flush) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            flush() -> $api::DurabilityReport);
    };
    (@method [$api:ident $($target:tt)+] shutdown) => {
        $crate::session_store_methods!(@future [$api $($target)+]
            shutdown() -> $api::DurabilityReport);
    };

    (@future [$api:ident forward $inner:ident]
        $name:ident($($arg:ident: $input:ty),*) -> $output:ty) => {
        fn $name(&self, $($arg: $input),*) -> $api::StoreFuture<'_, $output> {
            self.$inner.$name($($arg),*)
        }
    };
    (@future [$api:ident unavailable]
        $name:ident($($arg:ident: $input:ty),*) -> $output:ty) => {
        fn $name(&self, $(_: $input),*) -> $api::StoreFuture<'_, $output> {
            Box::pin(async { Err($api::SessionStoreError::BackendUnavailable) })
        }
    };
}
