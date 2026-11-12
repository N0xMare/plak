//! Process-local receipt fences, cross-runner quotas and isolated operation locks.

use super::{
    Arc, BTreeMap, OperatorNamespace, OperatorRefusal, State, TerminalError, TerminalKey,
    TerminalRegistry,
};

impl TerminalRegistry {
    pub(super) fn runner_operation(&self, runner: &str) -> Arc<tokio::sync::RwLock<()>> {
        let mut locks = self
            .operations
            .lock()
            .unwrap_or_else(super::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() != 0);
        if let Some(lock) = locks.get(runner).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::RwLock::new(()));
        locks.insert(runner.into(), Arc::downgrade(&lock));
        lock
    }

    pub(super) async fn namespace_operation(
        &self,
        runner: &str,
        namespace: &OperatorNamespace,
    ) -> (
        tokio::sync::OwnedRwLockReadGuard<()>,
        tokio::sync::OwnedMutexGuard<()>,
    ) {
        let runner_lock = self.runner_operation(runner).read_owned().await;
        let lock = {
            let mut locks = self
                .namespace_operations
                .lock()
                .unwrap_or_else(super::PoisonError::into_inner);
            locks.retain(|_, lock| lock.strong_count() != 0);
            let key = (runner.to_owned(), namespace.principal.as_str().to_owned());
            if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        (runner_lock, lock.lock_owned().await)
    }

    pub(super) fn admit_open(&self, namespace: &OperatorNamespace) -> Result<(), TerminalError> {
        self.admit_open_at(namespace, std::time::Instant::now())
    }

    pub(super) fn admit_open_at(
        &self,
        namespace: &OperatorNamespace,
        now: std::time::Instant,
    ) -> Result<(), TerminalError> {
        let mut state = self.lock();
        state.opened.retain(|_, times| {
            times.retain(|at| now.duration_since(*at) < std::time::Duration::from_secs(60));
            !times.is_empty()
        });
        let times = state
            .opened
            .entry(namespace.principal.as_str().into())
            .or_default();
        if times.len() >= 6 {
            let elapsed = now.duration_since(times[0]);
            let remaining = std::time::Duration::from_secs(60).saturating_sub(elapsed);
            let retry_after_ms = u64::try_from(remaining.as_nanos().div_ceil(1_000_000))
                .unwrap_or(60_000)
                .max(1);
            return Err(TerminalError::OpenRateLimited { retry_after_ms });
        }
        times.push_back(now);
        Ok(())
    }

    pub(super) fn observe_unbound(&self, runner: &str, client: &super::RunnerClient) -> bool {
        let mut state = self.lock();
        if client.is_connected() {
            state.unbound.remove(runner);
            return false;
        }
        let new = || super::Unbound {
            client: client.clone(),
            revision: client.connection_revision(),
            since: std::time::Instant::now(),
        };
        let observed = state.unbound.entry(runner.into()).or_insert_with(new);
        if !observed.client.same_client(client) || !observed.revision.is_unbound() {
            *observed = new();
        }
        observed.revision.is_unbound()
            && observed.since.elapsed() >= std::time::Duration::from_secs(90)
    }

    pub(super) fn receipt_key(
        generation: &str,
        key: &TerminalKey,
    ) -> Result<String, TerminalError> {
        Ok(plakous_hosted::workspace_maintenance::digest_text(
            &serde_json::to_string(&(generation, key)).map_err(|_| TerminalError::Conflict)?,
        ))
    }

    pub(super) fn receipt_fence(
        &self,
        namespace: &OperatorNamespace,
        key: &TerminalKey,
        digest: Option<&str>,
    ) -> Result<(), TerminalError> {
        let key = Self::receipt_key(&self.runner_generation, key)?;
        let state = self.lock();
        let Some(fences) = state.fences.get(namespace.principal.as_str()) else {
            return Ok(());
        };
        if let Some(prior) = fences.get(&key) {
            return Err(if digest.is_some_and(|digest| digest != prior) {
                TerminalError::Conflict
            } else {
                TerminalError::Lost
            });
        }
        if fences.len() >= 4096 {
            return Err(TerminalError::Refused(OperatorRefusal::Quota));
        }
        Ok(())
    }

    pub(super) fn collect_receipts(state: &mut State) {
        let mut completed: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
        for (id, entry) in &state.entries {
            if !entry.pinned && !state.controllers.contains_key(id) {
                completed
                    .entry(entry.pin.namespace.principal.as_str().into())
                    .or_default()
                    .push((entry.completed, id.clone()));
            }
        }
        for (principal, mut entries) in completed {
            entries.sort();
            let remove = entries.len().saturating_sub(8);
            for (_, id) in entries.into_iter().take(remove) {
                let Some(entry) = state.entries.remove(&id) else {
                    continue;
                };
                let fences = state.fences.entry(principal.clone()).or_default();
                if fences.len() < 4096
                    && let Ok(key) = Self::receipt_key(&entry.pin.generation, &entry.pin.key)
                {
                    fences.insert(key, entry.pin.digest);
                }
                // At fence saturation this principal's new opens are refused;
                // inventory import remains authoritative and cannot fail a bind.
            }
        }
    }
}
