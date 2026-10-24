//! Bounded lanes, queue, and the actor thread handle.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;

pub(super) const DEFAULT_QUEUE_CAPACITY: usize = 64;

pub(super) const READ_QUEUE_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) const SLOW_JOB_THRESHOLD: Duration = Duration::from_millis(250);

const BACKGROUND_JOB_INTERVAL: usize = 32;

pub(super) type Job = Box<dyn FnOnce(&mut Backend) + Send + 'static>;

#[derive(Clone, Copy)]
pub(super) enum Lane {
    Critical,
    Read,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExecutedJob {
    Critical,
    Read,
    Background,
}

impl Lane {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Read => "read",
        }
    }
}

/// The least time between two overflow warnings of one lane.
pub(super) const OVERFLOW_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Rate limit for one lane's queue-full warnings: the first overflow logs,
/// then at most one line per [`OVERFLOW_LOG_INTERVAL`] carrying the count.
#[derive(Default)]
pub(super) struct OverflowLog {
    pub(super) logged_at: Option<Instant>,
    pub(super) unlogged: u64,
}

impl OverflowLog {
    /// Counts one overflow at `now`; returns the overflows the caller logs
    /// now, including this one, or `None` while the interval holds them.
    pub(super) fn record(&mut self, now: Instant) -> Option<u64> {
        self.unlogged = self.unlogged.saturating_add(1);
        if self
            .logged_at
            .is_some_and(|at| now.saturating_duration_since(at) < OVERFLOW_LOG_INTERVAL)
        {
            return None;
        }
        self.logged_at = Some(now);
        Some(std::mem::take(&mut self.unlogged))
    }
}

pub(super) struct ActorActivity {
    pub(super) stopped: AtomicBool,
    pub(super) depths: [AtomicUsize; 2],
    pub(super) running: Mutex<Option<(&'static str, Instant)>>,
    pub(super) read_space: tokio::sync::Semaphore,
}

impl Default for ActorActivity {
    fn default() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            depths: Default::default(),
            running: Mutex::new(None),
            read_space: tokio::sync::Semaphore::new(0),
        }
    }
}

impl ActorActivity {
    pub(super) fn start(&self, label: &'static str) -> Instant {
        let started = Instant::now();
        *self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((label, started));
        started
    }

    pub(super) fn finish(&self, lane: &'static str, label: &'static str, started: Instant) {
        let elapsed = started.elapsed();
        *self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if elapsed >= SLOW_JOB_THRESHOLD {
            tracing::warn!(
                target: "plakous_store::sqlite_actor",
                lane,
                job = label,
                elapsed_ms = elapsed.as_millis(),
                critical_depth = self.depths[0].load(Ordering::Relaxed),
                read_depth = self.depths[1].load(Ordering::Relaxed),
                "SQLite store actor job is slow"
            );
        }
    }
}

#[derive(Clone)]
pub(super) struct ActorSender {
    critical: mpsc::SyncSender<Job>,
    read: mpsc::SyncSender<Job>,
    wake: mpsc::SyncSender<()>,
    pub(super) capacity: usize,
    overflow: Arc<[Mutex<OverflowLog>; 2]>,
    pub(super) activity: Arc<ActorActivity>,
}

pub(super) struct ActorReceiver {
    critical: mpsc::Receiver<Job>,
    pub(super) read: mpsc::Receiver<Job>,
    wake: mpsc::Receiver<()>,
    activity: Arc<ActorActivity>,
}

pub(super) fn actor_queue(capacity: usize) -> (ActorSender, ActorReceiver) {
    let activity = Arc::new(ActorActivity {
        read_space: tokio::sync::Semaphore::new(capacity),
        ..ActorActivity::default()
    });
    let (critical_tx, critical_rx) = mpsc::sync_channel(capacity);
    let (read_tx, read_rx) = mpsc::sync_channel(capacity);
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    (
        ActorSender {
            critical: critical_tx,
            read: read_tx,
            wake: wake_tx,
            capacity,
            overflow: Arc::default(),
            activity: Arc::clone(&activity),
        },
        ActorReceiver {
            critical: critical_rx,
            read: read_rx,
            wake: wake_rx,
            activity,
        },
    )
}

impl ActorSender {
    pub(super) fn enqueue(&self, lane: Lane, job: Job) -> Result<(), mpsc::TrySendError<Job>> {
        if self.activity.stopped.load(Ordering::Acquire) {
            return Err(mpsc::TrySendError::Disconnected(job));
        }
        let sender = match lane {
            Lane::Critical => &self.critical,
            Lane::Read => &self.read,
        };
        // Count before publication: the actor may consume the job immediately.
        self.activity.depths[lane as usize].fetch_add(1, Ordering::Relaxed);
        if let Err(error) = sender.try_send(job) {
            self.activity.depths[lane as usize].fetch_sub(1, Ordering::Relaxed);
            return Err(error);
        }
        let _ = self.wake.try_send(());
        Ok(())
    }

    pub(super) fn send(&self, lane: Lane, job: Job) -> Result<(), SessionStoreError> {
        let permit = if matches!(lane, Lane::Read) {
            Some(
                self.activity
                    .read_space
                    .try_acquire()
                    .map_err(|error| match error {
                        tokio::sync::TryAcquireError::Closed => {
                            SessionStoreError::BackendUnavailable
                        }
                        tokio::sync::TryAcquireError::NoPermits => {
                            self.overflowed(lane);
                            SessionStoreError::BackendBusy
                        }
                    })?,
            )
        } else {
            None
        };
        self.enqueue(lane, job)
            .map(|()| {
                if let Some(permit) = permit {
                    permit.forget();
                }
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    self.overflowed(lane);
                    SessionStoreError::BackendBusy
                }
                mpsc::TrySendError::Disconnected(_) => {
                    actor_lost(lane, "disconnected");
                    SessionStoreError::BackendUnavailable
                }
            })
    }

    pub(super) fn send_read(&self, job: Job) -> StoreFuture<'_, ()> {
        let first = match self.activity.read_space.try_acquire() {
            Ok(permit) => {
                let result = self.enqueue(Lane::Read, job).map_err(|_| {
                    actor_lost(Lane::Read, "disconnected");
                    SessionStoreError::BackendUnavailable
                });
                if result.is_ok() {
                    permit.forget();
                }
                return Box::pin(async move { result });
            }
            Err(error) => error,
        };
        Box::pin(async move {
            let permit = match first {
                tokio::sync::TryAcquireError::Closed => {
                    actor_lost(Lane::Read, "disconnected");
                    return Err(SessionStoreError::BackendUnavailable);
                }
                tokio::sync::TryAcquireError::NoPermits => {
                    match tokio::time::timeout(
                        READ_QUEUE_TIMEOUT,
                        self.activity.read_space.acquire(),
                    )
                    .await
                    {
                        Ok(Ok(permit)) => permit,
                        Ok(Err(_)) => {
                            actor_lost(Lane::Read, "disconnected");
                            return Err(SessionStoreError::BackendUnavailable);
                        }
                        Err(_) => {
                            self.overflowed(Lane::Read);
                            return Err(SessionStoreError::BackendBusy);
                        }
                    }
                }
            };
            self.enqueue(Lane::Read, job).map_err(|_| {
                actor_lost(Lane::Read, "disconnected");
                SessionStoreError::BackendUnavailable
            })?;
            // Queue capacity becomes available at dequeue, not at job completion.
            permit.forget();
            Ok(())
        })
    }
}

impl ActorSender {
    /// Queue pressure is visible without logging session or request content.
    fn overflowed(&self, lane: Lane) {
        let log = &self.overflow[lane as usize];
        let overflows = log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(Instant::now());
        if let Some(overflows) = overflows {
            let running = *self
                .activity
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (running_job, running_ms) =
                running.map_or(("idle", 0), |(label, at)| (label, at.elapsed().as_millis()));
            tracing::warn!(
                target: "plakous_store::sqlite_actor",
                lane = lane.label(),
                capacity = self.capacity,
                overflows,
                critical_depth = self.activity.depths[0].load(Ordering::Relaxed),
                read_depth = self.activity.depths[1].load(Ordering::Relaxed),
                running_job,
                running_ms,
                "SQLite store request lane is full"

            );
        }
    }
}

/// The actor thread exited (a job panicked): `disconnected` refuses a new
/// request, `reply_dropped` loses the reply of one already queued.
pub(super) fn actor_lost(lane: Lane, reason: &'static str) {
    tracing::warn!(
        target: "plakous_store::sqlite_actor",
        lane = lane.label(),
        reason,
        "SQLite store actor is gone; request refused as unavailable"
    );
}

impl Drop for ActorReceiver {
    fn drop(&mut self) {
        self.activity.stopped.store(true, Ordering::Release);
        self.activity.read_space.close();
    }
}

impl ActorReceiver {
    pub(super) fn background_step(&self, backend: &mut Backend) -> bool {
        let started = self.activity.start("background_step");
        let pending = backend.background_step();
        self.activity
            .finish("background", "background_step", started);
        pending
    }

    pub(super) fn run(&self, backend: &mut Backend) {
        let mut foreground_jobs = 0;
        loop {
            if foreground_jobs >= BACKGROUND_JOB_INTERVAL {
                self.background_step(backend);
                foreground_jobs = 0;
            }
            let job = self
                .critical
                .try_recv()
                .map(|job| (Lane::Critical, job))
                .or_else(|_| self.read.try_recv().map(|job| (Lane::Read, job)));
            if let Ok((lane, job)) = job {
                self.activity.depths[lane as usize].fetch_sub(1, Ordering::Relaxed);
                if matches!(lane, Lane::Read) {
                    self.activity.read_space.add_permits(1);
                }
                job(backend);
                foreground_jobs += 1;
                continue;
            }
            if self.background_step(backend) {
                foreground_jobs = 0;
                continue;
            }
            if self.wake.recv().is_err() {
                break;
            }
        }
    }
}

pub(super) struct Worker {
    pub(super) join: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Ok(slot) = self.join.get_mut()
            && let Some(join) = slot.take()
        {
            let _ = join.join();
        }
    }
}
