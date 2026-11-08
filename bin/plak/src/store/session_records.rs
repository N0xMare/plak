//! The product's SQLite implementation of the hosted session-record port.
//!
//! The port itself is `plakous_hosted::session_store`, re-exported here so a
//! consumer's `crate::store::session_records::…` path does not change twice.

use std::path::Path;
use std::sync::{Arc, Mutex};

use plakous_hosted::sessions::projector::{
    ProductIntents, ProjectionCommit, SessionViews, ViewWrites,
};

#[cfg(any(test, feature = "test-support"))]
pub use plakous_hosted::session_store::MemoryStore;
pub use plakous_hosted::session_store::SessionRecordStore;

use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;

type StorageInterrupt = Arc<dyn Fn() -> bool + Send + Sync>;

/// One lazily opened product database shared by command and hosted workers.
#[derive(Clone)]
pub struct SqliteSessionStore {
    paths: PlakPaths,
    connection: Arc<Mutex<Option<crate::store::ProductStore>>>,
    storage_override: Option<plakous_hosted::session_store::StorageAdmission>,
    /// This handle's operator interrupt; other handles keep their own.
    storage_interrupt: Option<StorageInterrupt>,
    storage_waiting: Arc<std::sync::atomic::AtomicBool>,
    capacity_retry_interval: std::time::Duration,
    #[cfg(test)]
    opens: Arc<std::sync::atomic::AtomicUsize>,
}

impl std::fmt::Debug for SqliteSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteSessionStore").finish_non_exhaustive()
    }
}

impl SqliteSessionStore {
    /// The store behind `paths`' product database.
    #[must_use]
    pub fn new(paths: PlakPaths) -> Self {
        Self {
            paths,
            connection: Arc::new(Mutex::new(None)),
            storage_interrupt: None,
            storage_waiting: Arc::default(),
            capacity_retry_interval: std::time::Duration::from_secs(5),
            #[cfg(test)]
            storage_override: Some(plakous_hosted::session_store::StorageAdmission::Available),
            #[cfg(not(test))]
            storage_override: None,
            #[cfg(test)]
            opens: Arc::default(),
        }
    }

    /// Inject capacity independently of the fixture's filesystem.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_storage_admission(
        mut self,
        admission: plakous_hosted::session_store::StorageAdmission,
    ) -> Self {
        self.storage_override = Some(admission);
        self
    }

    #[cfg(test)]
    pub(crate) fn opened_connections(&self) -> usize {
        self.opens.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn with_store<T, E: From<String>>(
        &self,
        operation: impl FnOnce(&mut crate::store::ProductStore) -> Result<T, E>,
    ) -> Result<T, E> {
        // A panic rolls SQLite transactions back before the mutex unlocks.
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Retirement and external replacement must never leave this service
        // writing the detached database through its old file descriptor.
        if connection
            .as_ref()
            .is_some_and(|store| !store.matches_path(&self.paths.product_database()))
        {
            *connection = None;
        }
        if connection.is_none() {
            *connection = Some(
                crate::store::ProductStore::open(&self.paths)
                    .map_err(|error| E::from(error.to_string()))?,
            );
            #[cfg(test)]
            self.opens
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let store = connection
            .as_mut()
            .ok_or_else(|| E::from("product store is unavailable".to_owned()))?;
        operation(store)
    }

    fn with_write<T>(
        &self,
        mut operation: impl FnMut(
            &mut crate::store::ProductStore,
        ) -> Result<T, plakous_hosted::session_store::RecordStoreError>,
    ) -> Result<T, plakous_hosted::session_store::RecordStoreError> {
        loop {
            let result = self.with_store(&mut operation);
            if result.is_ok() {
                self.storage_waiting
                    .store(false, std::sync::atomic::Ordering::Release);
            }
            if !matches!(&result, Err(error) if error.refusal() == Some(plakous_hosted::error::StoreRefusal::StorageFull))
            {
                return result;
            }
            self.storage_waiting
                .store(true, std::sync::atomic::Ordering::Release);
            let next = std::time::Instant::now() + self.capacity_retry_interval;
            // No product connection or write gate is held here. Other
            // owners can read health and the operator can interrupt the wait.
            let wait = || {
                while std::time::Instant::now() < next {
                    if self.storage_wait_cancelled() {
                        return false;
                    }
                    std::thread::sleep(
                        next.saturating_duration_since(std::time::Instant::now())
                            .min(std::time::Duration::from_millis(100)),
                    );
                }
                !self.storage_wait_cancelled()
            };
            let retry = if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
                handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
            }) {
                tokio::task::block_in_place(wait)
            } else {
                wait()
            };
            if !retry {
                return result;
            }
        }
    }
}

impl SessionRecordStore for SqliteSessionStore {
    fn with_storage_interrupt(
        self: Arc<Self>,
        probe: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Arc<dyn SessionRecordStore> {
        Arc::new(Self {
            storage_interrupt: Some(probe),
            ..(*self).clone()
        })
    }

    fn storage_wait_cancelled(&self) -> bool {
        self.storage_interrupt.as_ref().is_some_and(|probe| probe())
    }

    fn recover_storage(&self) -> Result<(), plakous_hosted::session_store::RecordStoreError> {
        self.with_store(super::ProductStore::recover_storage)?;
        self.storage_waiting
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn storage_outage_notice(&self) -> String {
        crate::storage_space::outage_notice(self.paths.home())
    }

    fn storage_unavailable(&self) -> bool {
        self.storage_waiting
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn storage_admission(&self) -> plakous_hosted::session_store::StorageAdmission {
        if let Some(admission) = &self.storage_override {
            return admission.clone();
        }
        crate::storage_space::admission(self.paths.home())
    }

    fn emit_projected_turn_usages(
        &self,
        id: &str,
        terminals: &[plakous_hosted::sessions::projector::ProjectedTurnUsage],
    ) {
        if terminals.is_empty() {
            return;
        }
        let record = self
            .with_store::<_, String>(|store| store.load_turn_attribution(id))
            .ok();
        crate::telemetry_store::emit_projected_turn_usages(&self.paths, record.as_ref(), terminals);
    }
    fn write_host_controls(
        &self,
        id: &str,
        suppress_wake: bool,
        pause: Option<plakous_hosted::sessions::projector::HostPauseCause>,
    ) -> Result<(), plakous_hosted::session_store::RecordStoreError> {
        self.with_write(|store| store.write_host_controls(id, suppress_wake, pause.clone()))
    }
    fn write_root_loss(
        &self,
        id: &str,
        intent: plakous_hosted::sessions::projector::RootLossIntent,
    ) -> Result<(), plakous_hosted::session_store::RecordStoreError> {
        crate::store::sessions::canonical_product_session_id(id)?;
        self.with_write(|store| store.write_root_loss_at_cursor(id, &intent))
    }
    fn load(&self, id: &str) -> Result<SessionRecord, String> {
        crate::store::sessions::canonical_product_session_id(id)?;
        self.with_store(|store| store.load(id))
    }

    fn load_queue_head(
        &self,
        id: &str,
    ) -> Result<plakous_hosted::session_store::SessionQueueHead, String> {
        crate::store::sessions::canonical_product_session_id(id)?;
        self.with_store(|store| store.load_queue_head(id))
    }

    fn save(
        &self,
        session: &mut SessionRecord,
    ) -> Result<(), plakous_hosted::session_store::RecordStoreError> {
        self.with_write(|store| store.commit(session))
    }

    fn save_submission(
        &self,
        session: &mut SessionRecord,
        submission: &plakous_hosted::sessions::projector::Submission,
    ) -> Result<(), plakous_hosted::session_store::RecordStoreError> {
        self.with_write(|store| store.commit_submission(session, submission))
    }

    fn discard_submission(
        &self,
        session: &mut SessionRecord,
        command_id: &str,
        pause_cause: Option<plakous_hosted::sessions::projector::HostPauseCause>,
    ) -> Result<(), plakous_hosted::session_store::RecordStoreError> {
        self.with_write(|store| store.discard_submission(session, command_id, pause_cause.clone()))
    }

    fn submit_workflow_request(
        &self,
        session: &mut SessionRecord,
        request: &plakous_hosted::sessions::projector::WorkflowRequest,
    ) -> Result<(), plakous_hosted::session_store::RecordStoreError> {
        self.with_write(|store| store.submit_workflow_request(session, request))
    }

    fn discard_unadmitted_workflow(
        &self,
        session: &mut SessionRecord,
        operation_id: &str,
    ) -> Result<(), plakous_hosted::session_store::RecordStoreError> {
        self.with_write(|store| store.discard_unadmitted_workflow(session, operation_id))
    }

    fn withdraw_unadmitted_workflows(
        &self,
        session: &mut SessionRecord,
    ) -> Result<
        Vec<plakous_hosted::sessions::TranscriptRow>,
        plakous_hosted::session_store::RecordStoreError,
    > {
        self.with_write(|store| store.withdraw_unadmitted_workflows(session))
    }

    fn list_for_workspace(&self, workspace: &Path) -> Result<Vec<SessionRecord>, String> {
        self.with_store(|store| store.list_for_workspace(workspace))
    }

    fn load_intents(&self, id: &str) -> Result<ProductIntents, String> {
        self.with_store(|store| store.load_intents(id))
    }

    fn load_inherited_submissions(
        &self,
        id: &str,
    ) -> Result<Vec<plakous_hosted::sessions::projector::Submission>, String> {
        self.with_store(|store| store.load_inherited_submissions(id))
    }

    fn load_views(&self, id: &str) -> Result<Option<SessionViews>, String> {
        self.with_store(|store| store.load_views(id))
    }

    fn load_queue_commands(
        &self,
        id: &str,
    ) -> Result<plakous_hosted::sessions::QueueCommands, String> {
        self.with_store(|store| store.load_queue_commands(id))
    }

    fn commit_projection_views(
        &self,
        id: &str,
        writes: &ViewWrites,
        views: &plakous_hosted::sessions::projector::SessionViewState,
    ) -> Result<ProjectionCommit, plakous_hosted::session_store::RecordStoreError> {
        self.with_write(|store| store.commit_projection_views(id, writes, views))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod contract;
