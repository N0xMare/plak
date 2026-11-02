//! Shared content-free reachability, keyed by the exact model route.

use plakous::model::{CancellationToken, ReachabilityProbe, RouteId};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::watch;

/// A host's shared network wait service. No credentials or HTTP bytes are sent.
#[derive(Default)]
pub struct NetworkWatch {
    routes: Mutex<BTreeMap<RouteId, Weak<NetworkTask>>>,
    /// Probes are shared by every subscriber, so the service owns them, not
    /// whichever session or tree subscribed first.
    tasks: crate::TaskSet,
}

struct NetworkTask {
    cancellation: CancellationToken,
    ready: watch::Receiver<bool>,
}

impl Drop for NetworkTask {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl NetworkWatch {
    /// The service shared by the hosted root and child owners in this process.
    pub fn shared() -> &'static Self {
        static SERVICE: std::sync::OnceLock<NetworkWatch> = std::sync::OnceLock::new();
        SERVICE.get_or_init(Self::default)
    }

    fn subscribe(&self, probe: ReachabilityProbe) -> Arc<NetworkTask> {
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        routes.retain(|_, task| task.strong_count() != 0);
        if let Some(task) = routes.get(probe.route()).and_then(Weak::upgrade) {
            return task;
        }
        let (ready, receiver) = watch::channel(false);
        let task = Arc::new(NetworkTask {
            cancellation: CancellationToken::new(),
            ready: receiver,
        });
        routes.insert(probe.route().clone(), Arc::downgrade(&task));
        let cancellation = task.cancellation.clone();
        self.tasks.spawn(async move {
            let mut backoff = Duration::from_secs(5);
            loop {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                let result = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return,
                    result = tokio::time::timeout_at(deadline, probe.run(cancellation.clone(), deadline.into_std())) => result,
                };
                if matches!(result, Ok(Ok(()))) {
                    ready.send_replace(true);
                    return;
                }
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return,
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        });
        task
    }
    /// Waits for exact-route readiness, or a bounded fallback when the model
    /// has no probe. Cancellation releases this subscriber, not other waiters.
    pub(crate) async fn wait(
        &self,
        probe: Option<ReachabilityProbe>,
        fallback: Duration,
        cancellation: &CancellationToken,
    ) -> bool {
        let Some(probe) = probe else {
            return tokio::select! {
                biased;
                () = cancellation.cancelled() => false,
                () = tokio::time::sleep(fallback) => true,
            };
        };
        if cancellation.is_cancelled() {
            return false;
        }
        let task = self.subscribe(probe);
        let mut ready = task.ready.clone();
        loop {
            if cancellation.is_cancelled() {
                return false;
            }
            if *ready.borrow_and_update() {
                return true;
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return false,
                changed = ready.changed() => if changed.is_err() { return *ready.borrow(); }
            }
        }
    }
}

#[cfg(test)]
mod tests;
