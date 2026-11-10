//! Explicit runner backend and executor, shared without selecting process authority.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use super::{RunnerBackend, RunnerWorkspaceProbe};
use crate::WorkspaceProbe;

/// One selected runner: its backend, and the runtime its link runs on.
#[derive(Clone)]
pub struct RunnerServices {
    backend: Arc<RunnerBackend>,
    runtime: tokio::runtime::Handle,
    availability: Arc<watch::Sender<Availability>>,
    reconcile: Reconcile,
}

type Reconcile = Arc<Mutex<Option<Arc<dyn Fn() -> bool + Send + Sync>>>>;

/// Whether the placement admits new calls: its owner opens it after the
/// first-bind/reconnect barrier, holds it while the link or that barrier is
/// down, and refuses it for good once the placement itself was refused or
/// replaced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Admission {
    Open,
    Held,
    Refused,
}

/// The admission and its revision: every change advances the revision, so
/// a waiter whose call found the link gone never mistakes the stale open
/// state it was sent under for a fresh one.
#[derive(Clone, Copy, Debug)]
struct Availability {
    admission: Admission,
    revision: u64,
    held_since: Option<std::time::Instant>,
    held_elapsed: Duration,
}

impl RunnerServices {
    /// Explicit services of one runner; construction selects no process global.
    #[must_use]
    pub fn new(backend: Arc<RunnerBackend>, runtime: tokio::runtime::Handle) -> Self {
        let availability = Arc::new(
            watch::channel(Availability {
                admission: Admission::Open,
                revision: 0,
                held_since: None,
                held_elapsed: Duration::ZERO,
            })
            .0,
        );
        let reconcile = Reconcile::default();
        let link = Arc::new(AdmittedLink {
            inner: backend.link(),
            availability: Arc::clone(&availability),
            reconcile: Arc::clone(&reconcile),
        });
        let mut backend = Arc::unwrap_or_clone(backend).running_on(runtime.clone());
        backend.link = link;
        Self {
            backend: Arc::new(backend),
            runtime,
            availability,
            reconcile,
        }
    }

    /// Opens or holds new-call admission across reconnect reconciliation
    /// and configuration. Existing reads and admitted futures retain their
    /// recovery owner; held model calls wait (spec 01 §8.4). Opening also
    /// lifts an earlier refusal for calls made from here on.
    pub fn set_available(&self, available: bool) {
        self.availability.send_modify(|state| {
            if available {
                state.end_hold();
            } else if state.admission != Admission::Refused && state.held_since.is_none() {
                state.held_since = Some(std::time::Instant::now());
            }
            state.admission = match (available, state.admission) {
                (true, _) => Admission::Open,
                (false, Admission::Refused) => Admission::Refused,
                (false, _) => Admission::Held,
            };
            state.revision = state.revision.wrapping_add(1);
        });
    }

    /// Ends every placement hold on these services with a permanent
    /// refusal: the placement was refused, removed or replaced. New calls
    /// refuse until [`Self::set_available`] opens admission again.
    pub fn refuse(&self) {
        self.availability.send_modify(|state| {
            state.end_hold();
            state.admission = Admission::Refused;
            state.revision = state.revision.wrapping_add(1);
        });
    }

    /// Whether the owner has opened the new-call admission barrier.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.availability.borrow().admission == Admission::Open
    }

    /// Installs how a held call asks its owner for a fresh reconciliation
    /// when the link refused it under an admission that still reads open.
    /// The request answers whether it acted; a coalesced request that did
    /// not leaves the call to its own bounded recheck.
    pub fn request_reconciliation_with(&self, request: impl Fn() -> bool + Send + Sync + 'static) {
        *self
            .reconcile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(request));
    }

    /// Whether a built/open node still retains these services.
    #[must_use]
    pub fn has_node_users(&self) -> bool {
        Arc::strong_count(&self.backend) > 1
    }

    /// Workspace probe of this runner, never the host filesystem.
    #[must_use]
    pub fn workspace_probe(&self) -> Arc<dyn WorkspaceProbe> {
        Arc::new(RunnerWorkspaceProbe::new(
            self.backend.link(),
            self.runtime.clone(),
        ))
    }
    /// The backend sessions run their operations on.
    #[must_use]
    pub fn backend(&self) -> Arc<RunnerBackend> {
        Arc::clone(&self.backend)
    }

    /// Transfers the explicit live backend and executor to a facade builder.
    #[must_use]
    pub fn into_parts(self) -> (Arc<RunnerBackend>, tokio::runtime::Handle) {
        (self.backend, self.runtime)
    }

    /// The multi-thread runtime the backend's link runs on.
    #[must_use]
    pub const fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }
}

/// A placed runner's hold as a driver sees it: whether new paid model work
/// must wait, and when it may go on. It names no runner or topology.
#[derive(Clone, Debug)]
pub struct PlacementGate {
    availability: watch::Receiver<Availability>,
}

impl PlacementGate {
    /// Cumulative, non-overlapping host-side hold time on these services.
    #[must_use]
    pub fn held_elapsed(&self) -> Duration {
        let state = self.availability.borrow();
        state.held_elapsed.saturating_add(
            state
                .held_since
                .map_or(Duration::ZERO, |since| since.elapsed()),
        )
    }
    /// Whether the current barrier admits new work; refusal is not readiness.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.availability.borrow().admission == Admission::Open
    }

    /// Observes the next availability revision without polling a tool call.
    pub async fn changed(&mut self) {
        if self.availability.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Whether the placement is held: the link or its reconnect barrier is
    /// down, and its owner has not refused it.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.availability.borrow().admission == Admission::Held
    }

    /// Waits until the placement is no longer held: released after the
    /// barrier, or refused.
    pub async fn released(&mut self) {
        let _ = self
            .availability
            .wait_for(|state| state.admission != Admission::Held)
            .await;
    }
}

impl Availability {
    fn end_hold(&mut self) {
        if let Some(since) = self.held_since.take() {
            self.held_elapsed = self.held_elapsed.saturating_add(since.elapsed());
        }
    }
}

struct AdmittedLink {
    inner: Arc<dyn super::RunnerLink>,
    availability: Arc<watch::Sender<Availability>>,
    reconcile: Reconcile,
}

impl AdmittedLink {
    /// A call that never waits: under a closed admission, a read still goes
    /// through, and anything else is refused unsent.
    fn unheld(&self, call: &super::RunnerCall) -> Option<super::RunnerFailure> {
        if call.verb().is_read() {
            return None;
        }
        match self.availability.borrow().admission {
            Admission::Open => None,
            Admission::Held => Some(super::RunnerFailure::NotSent(
                "runner is blocked before dispatch".into(),
            )),
            Admission::Refused => Some(refused()),
        }
    }

    fn hold(&self) -> Hold {
        Hold {
            availability: self.availability.subscribe(),
            reconcile: Arc::clone(&self.reconcile),
        }
    }
}

/// How long a held call whose services have no reconciliation owner waits
/// before trying the same open admission again.
const RECHECK: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
];

fn refused() -> super::RunnerFailure {
    super::RunnerFailure::Unavailable(
        "the runner placement was refused or replaced before the call was sent".into(),
    )
}

/// One held call's view of its placement.
struct Hold {
    availability: watch::Receiver<Availability>,
    reconcile: Reconcile,
}

impl Hold {
    /// Sends `call` once its placement is open, and again with the same key
    /// after each transient refusal before send, or, for a read, after the
    /// link lost it. The wait runs here, outside every lock the link takes
    /// for the call, and ends at once on cancellation or refusal.
    async fn send<T>(
        mut self,
        call: super::RunnerCall,
        send: impl Fn(
            super::RunnerCall,
        )
            -> std::pin::Pin<Box<dyn Future<Output = Result<T, super::RunnerFailure>> + Send>>,
    ) -> Result<T, super::RunnerFailure> {
        let rereads = call.verb().is_read();
        let mut stale = None;
        let mut recheck = None;
        let mut step = 0;
        let mut held = Duration::ZERO;
        loop {
            let waiting = tokio::time::Instant::now();
            let revision = self.open(stale, recheck, call.cancellation()).await?;
            held = held.saturating_add(waiting.elapsed());
            match send(call.reissue(held)).await {
                Err(super::RunnerFailure::NotSent(_)) => {}
                Err(super::RunnerFailure::ReadInterrupted(_)) if rereads => {}
                answer => return answer,
            }
            stale = Some(revision);
            let current = self.availability.borrow().revision;
            let request = self
                .reconcile
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            // Unless its owner reconciles afresh, the same open admission
            // is tried again only after a bounded backoff: with no owner to
            // ask, or one whose request was coalesced away.
            let reconciling = current != revision || request.is_some_and(|request| request());
            recheck = (!reconciling).then(|| RECHECK[step]);
            step = (step + 1).min(RECHECK.len() - 1);
        }
    }

    /// The revision of an open admission other than `stale`, or the stale
    /// one again once `recheck` has passed.
    async fn open(
        &mut self,
        stale: Option<u64>,
        recheck: Option<Duration>,
        cancellation: &plakous_model::CancellationToken,
    ) -> Result<u64, super::RunnerFailure> {
        let mut recheck_at = recheck.map(|delay| tokio::time::Instant::now() + delay);
        loop {
            let current = *self.availability.borrow_and_update();
            match current.admission {
                Admission::Refused => return Err(refused()),
                Admission::Open if stale != Some(current.revision) => return Ok(current.revision),
                _ => {}
            }
            let woke = {
                let mut cancelled = std::pin::pin!(cancellation.cancelled());
                let mut changed = std::pin::pin!(self.availability.changed());
                let mut rechecked = std::pin::pin!(async move {
                    match recheck_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                });
                std::future::poll_fn(|context| {
                    if cancelled.as_mut().poll(context).is_ready() {
                        return std::task::Poll::Ready(None);
                    }
                    if rechecked.as_mut().poll(context).is_ready() {
                        return std::task::Poll::Ready(Some(Ok(true)));
                    }
                    changed
                        .as_mut()
                        .poll(context)
                        .map(|changed| Some(changed.map(|()| false)))
                })
                .await
            };
            match woke {
                Some(Ok(true)) => {
                    let now = *self.availability.borrow();
                    if now.admission == Admission::Open {
                        return Ok(now.revision);
                    }
                    // Not open: wait for a change, no longer on the timer.
                    recheck_at = None;
                }
                None => {
                    return Err(super::RunnerFailure::NotSent(
                        "the call was cancelled before it was sent".into(),
                    ));
                }
                Some(Err(_)) => return Err(refused()),
                Some(Ok(false)) => {}
            }
        }
    }
}

impl super::RunnerLink for AdmittedLink {
    fn driver_operation_settled(
        &self,
        owner: crate::LineOwner,
        call_id: plakous_model::CallId,
        attempt_id: plakous_model::AttemptId,
    ) {
        self.inner
            .driver_operation_settled(owner, call_id, attempt_id);
    }

    fn call(&self, call: super::RunnerCall) -> super::RunnerCallFuture {
        if !call.holds_for_placement() {
            if let Some(failure) = self.unheld(&call) {
                return Box::pin(std::future::ready(Err(failure)));
            }
            return self.inner.call(call);
        }
        let inner = Arc::clone(&self.inner);
        Box::pin(self.hold().send(call, move |call| inner.call(call)))
    }

    fn open(&self, call: super::RunnerCall) -> super::RunnerOpenFuture {
        if !call.holds_for_placement() {
            if let Some(failure) = self.unheld(&call).or_else(|| {
                (!self.is_open()).then(|| {
                    super::RunnerFailure::NotSent("runner is blocked before dispatch".into())
                })
            }) {
                return Box::pin(std::future::ready(Err(failure)));
            }
            return self.inner.open(call);
        }
        let inner = Arc::clone(&self.inner);
        Box::pin(self.hold().send(call, move |call| inner.open(call)))
    }

    fn placement(&self) -> Option<PlacementGate> {
        Some(PlacementGate {
            availability: self.availability.subscribe(),
        })
    }
}

impl AdmittedLink {
    fn is_open(&self) -> bool {
        self.availability.borrow().admission == Admission::Open
    }
}

impl std::fmt::Debug for RunnerServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerServices")
            .finish_non_exhaustive()
    }
}
