//! One session a host serves, resident or dormant (spec 02 §4.4). A
//! resident session is its host worker and the [`WireSession`] its
//! connections share. The session releases itself once nothing keeps it
//! resident (its actor's idle release); the slot then joins the worker and
//! holds only the session's id. The next trigger, a connection that wants
//! the session, materializes it again through the exact reopen and serves
//! it as before. A trigger takes its hold on the session's residency here,
//! under the slot's lock, so a session is never handed to a connection
//! after it began to release: a trigger that finds it releasing waits for
//! the release to commit, then opens it.
//!
//! A release that does not join within `RELEASE_JOIN` (60 s) fails the
//! trigger that waited for it and leaves the slot dormant, its worker
//! running on unjoined to its own end. The next trigger opens the session by id, and
//! that open is refused for as long as the old worker holds the session's
//! runtime lease, so a slow release never yields a second live session.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::NodeAddress;
use plakous_host_protocol::{IntentSender, ObservationReceiver, ProtocolExtension};

use super::super::{AttachmentCount, ResidencyHold};
use super::WireSession;
use crate::error::{CliFailure, CliResult};
use crate::runtime::HostWorkerHandle;

mod lifecycle;

/// How long a release may take to join before the slot gives up on it.
const RELEASE_JOIN: Duration = Duration::from_secs(60);

/// A session opened on a host worker: what the slot serves it with.
pub struct OpenedSession<P: ProtocolExtension> {
    /// Read-only runner link/reconciliation debt after durable cancellation.
    pub runner_pending: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// The session's root node.
    pub root: NodeAddress,
    /// The session's id.
    pub session_id: String,
    /// The session's mailbox.
    pub actions: IntentSender<P>,
    /// The session's observations.
    pub events: ObservationReceiver<P>,
    /// The worker the session runs on.
    pub worker: HostWorkerHandle,
    /// The attachments the session's residency reads; the slot's service
    /// counts into it.
    pub attachments: AttachmentCount,
    /// Finds the root of a session this one switches to.
    pub roots: RootResolver,
}

/// Finds a session's root node by its id.
pub type RootResolver = Box<dyn Fn(&str) -> Option<NodeAddress> + Send + Sync>;

/// Opens the session of an id on a fresh host worker.
type Opener<P> = Box<dyn Fn(&str) -> CliResult<OpenedSession<P>> + Send + Sync>;

type HeldSession<P> = (Arc<WireSession<P>>, ResidencyHold, u64);

enum Slot<P: ProtocolExtension> {
    Resident {
        session: Arc<WireSession<P>>,
        worker: HostWorkerHandle,
        attachments: AttachmentCount,
    },
    Dormant {
        session_id: String,
    },
}

/// One session a host serves, resident or dormant.
pub struct SessionSlot<P: ProtocolExtension> {
    slot: Mutex<Slot<P>>,
    open: Opener<P>,
    release_join: Duration,
    /// What the slot is doing while its lock is held for an open or a
    /// release join, for a reader that must not wait ([`Self::peek`]).
    busy: AtomicU8,
    /// The session the slot last served or opened, for the same reader.
    known: Mutex<String>,
    /// Last open failure, visible without waiting for a retry's slot lock.
    failure: Mutex<Option<CliFailure>>,
}

/// The slot's lock is held for no long step.
const BUSY_NONE: u8 = 0;
/// The slot's lock is held while the session opens.
const BUSY_OPENING: u8 = 1;
/// The slot's lock is held while a release joins.
const BUSY_RELEASING: u8 = 2;

/// A slot as a reader that must not wait sees it ([`SessionSlot::peek`]),
/// with the session it serves or will open next.
pub enum SlotView<P: ProtocolExtension> {
    /// Released; the next trigger opens the session of this id.
    Dormant(String),
    /// The session of this id is opening for a trigger.
    Opening(String),
    /// Open, and served by this service.
    Resident(Arc<WireSession<P>>),
    /// The session of this id is releasing, or its release is joining.
    Releasing(String),
}

impl<P: ProtocolExtension> SessionSlot<P>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
{
    /// A slot serving `opened`; `open` materializes its session again, by
    /// id, after it went dormant.
    #[must_use]
    pub fn new(
        opened: OpenedSession<P>,
        open: impl Fn(&str) -> CliResult<OpenedSession<P>> + Send + Sync + 'static,
    ) -> Self {
        let known = Mutex::new(opened.session_id.clone());
        Self {
            slot: Mutex::new(resident(opened)),
            open: Box::new(open),
            release_join: RELEASE_JOIN,
            busy: AtomicU8::new(BUSY_NONE),
            known,
            failure: Mutex::new(None),
        }
    }

    /// A slot of the dormant session `session_id`, which `open`
    /// materializes on its first trigger.
    #[must_use]
    pub fn released(
        session_id: impl Into<String>,
        open: impl Fn(&str) -> CliResult<OpenedSession<P>> + Send + Sync + 'static,
    ) -> Self {
        let session_id = session_id.into();
        Self {
            known: Mutex::new(session_id.clone()),
            slot: Mutex::new(Slot::Dormant { session_id }),
            open: Box::new(open),
            release_join: RELEASE_JOIN,
            busy: AtomicU8::new(BUSY_NONE),
            failure: Mutex::new(None),
        }
    }

    /// The most recent failed open, cleared by a successful open.
    #[must_use]
    pub(super) fn open_failure(&self) -> Option<CliFailure> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The slot as it stands, without waiting for an open or a release
    /// join that holds it.
    #[must_use]
    pub fn peek(&self) -> SlotView<P> {
        let slot = match self.slot.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                let known = self.known();
                return match self.busy.load(Ordering::Acquire) {
                    BUSY_RELEASING => SlotView::Releasing(known),
                    // Opening, or held a moment for a trigger's hold.
                    _ => SlotView::Opening(known),
                };
            }
        };
        match &*slot {
            Slot::Dormant { session_id } => SlotView::Dormant(session_id.clone()),
            Slot::Resident {
                session,
                attachments,
                worker,
            } => {
                if session.ended() || attachments.releasing() {
                    // A release whose worker ended is dormant; the next
                    // trigger only joins it.
                    if worker.is_finished() {
                        SlotView::Dormant(session.session_id())
                    } else {
                        SlotView::Releasing(session.session_id())
                    }
                } else {
                    SlotView::Resident(Arc::clone(session))
                }
            }
        }
    }

    /// The session the slot last served or opened, without waiting.
    #[must_use]
    pub fn known(&self) -> String {
        self.known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(super) fn serves_current(&self, expected: &str) -> bool {
        match &*self.lock() {
            Slot::Resident { session, .. } => session.session_id() == expected,
            Slot::Dormant { session_id } => session_id == expected,
        }
    }

    fn set_known(&self, session_id: &str) {
        session_id.clone_into(&mut self.known.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// This slot, giving a release `release_join` to join.
    #[cfg(test)]
    pub(crate) const fn with_release_join(mut self, release_join: Duration) -> Self {
        self.release_join = release_join;
        self
    }

    /// The resident session, held resident for the caller until it drops
    /// the hold: the one served now, or, when it released itself or is
    /// releasing, the same session (or the one it last switched to) opened
    /// again. Blocks while a release joins or an open runs.
    ///
    /// # Errors
    ///
    /// The release did not join in time, or the session does not open.
    pub fn resident(&self) -> CliResult<(Arc<WireSession<P>>, ResidencyHold)> {
        self.resident_at_handoff()
            .map(|(session, hold, _releases)| (session, hold))
    }

    /// [`Self::resident`], and how many releases had already been asked when
    /// the slot handed the session over. That count is read under the slot
    /// lock, so a release that waited out this open is still ahead of it and
    /// the attachment it makes detaches.
    pub(super) fn resident_at_handoff(&self) -> CliResult<HeldSession<P>> {
        let mut slot = self.lock();
        loop {
            if let Slot::Resident { session, .. } = &*slot {
                self.set_known(&session.session_id());
            }
            self.busy.store(BUSY_RELEASING, Ordering::Release);
            let settled = settle(&mut slot, self.release_join);
            self.busy.store(BUSY_NONE, Ordering::Release);
            settled?;
            if let Slot::Dormant { session_id } = &*slot {
                // On a thread of its own: an open builds the session's own
                // executor, which a caller's async runtime would refuse.
                self.set_known(session_id);
                self.busy.store(BUSY_OPENING, Ordering::Release);
                let opened = self.open_on_thread(session_id);
                self.busy.store(BUSY_NONE, Ordering::Release);
                *self.failure.lock().unwrap_or_else(PoisonError::into_inner) =
                    opened.as_ref().err().cloned();
                *slot = resident(opened?);
            }
            if let Slot::Resident {
                session,
                attachments,
                ..
            } = &*slot
                && let Some(hold) = attachments.hold()
            {
                let releases = session.releases();
                return Ok((Arc::clone(session), hold, releases));
            }
            // It began to release since it was settled: join it, then open
            // it again.
        }
    }

    /// Whether the session is dormant now: released, or releasing.
    #[must_use]
    pub fn dormant(&self) -> bool {
        match &*self.lock() {
            Slot::Resident {
                session,
                attachments,
                ..
            } => session.ended() || attachments.releasing(),
            Slot::Dormant { .. } => true,
        }
    }

    pub(super) fn stop_target(
        &self,
        expected: &str,
        stop_dormant: impl FnOnce(&str) -> CliResult<()> + Send,
    ) -> CliResult<Option<HeldSession<P>>> {
        let mut slot = self.lock();
        let serves = match &*slot {
            Slot::Resident { session, .. } => session.session_id() == expected,
            Slot::Dormant { session_id } => session_id == expected,
        };
        if !serves {
            return Err(CliFailure::new(
                "the named session moved; stop its current session explicitly",
            ));
        }
        if let Slot::Resident {
            session,
            attachments,
            ..
        } = &*slot
            && !session.ended()
            && let Some(hold) = attachments.hold()
        {
            return Ok(Some((Arc::clone(session), hold, session.releases())));
        }
        let source = match &*slot {
            Slot::Resident { session, .. } => session.session_id(),
            Slot::Dormant { session_id } => session_id.clone(),
        };
        if let Slot::Resident { worker, .. } = std::mem::replace(
            &mut *slot,
            Slot::Dormant {
                session_id: source.clone(),
            },
        ) {
            join(worker, self.release_join)?;
        }
        std::thread::scope(|scope| scope.spawn(|| stop_dormant(&source)).join())
            .map_err(|_| CliFailure::new("the dormant stop panicked"))??;
        Ok(None)
    }

    /// Quits a resident session and joins its worker within `deadline`; a
    /// dormant one has nothing to quit.
    ///
    /// # Errors
    ///
    /// The worker did not end in time, or ended with a failure.
    pub fn quit(&self, deadline: Duration) -> CliResult<()> {
        self.end(deadline, WireSession::quit)
    }

    /// Releases a resident session as a planned handoff (a host's stop):
    /// each node with work in flight is recorded handed off before the
    /// release, so the next host continues it once by itself; then joins
    /// its worker within `deadline`. A dormant one has nothing to hand off.
    ///
    /// # Errors
    ///
    /// The worker did not end in time, or ended with a failure.
    pub fn hand_off(&self, deadline: Duration) -> CliResult<()> {
        self.end(deadline, WireSession::hand_off)
    }

    /// Serializes an actor-fenced release, product carry/re-resolution and
    /// reopen with every other trigger for this slot.
    pub(super) fn reopen(
        &self,
        expected: &Arc<WireSession<P>>,
        expected_session: &str,
        attachment: u64,
        request: &str,
        change: impl FnOnce(&str) -> CliResult<(String, NodeAddress)> + Send,
    ) -> CliResult<NodeAddress> {
        let mut slot = self.lock();
        let Slot::Resident { session, .. } = &*slot else {
            return Err(CliFailure::new("the session is no longer resident"));
        };
        if !Arc::ptr_eq(session, expected) || session.session_id() != expected_session {
            return Err(CliFailure::new(
                "the requesting attachment's session already moved",
            ));
        }
        let old = Arc::clone(session);
        let ticket = old.begin_reopen(expected_session)?;
        // Only the actor decides whether releasing is admissible. Once it
        // acknowledges, join that same worker before any target is built.
        let until = Instant::now() + self.release_join;
        loop {
            if let Some(result) = old.reopen_settled(ticket) {
                if let Err(detail) = result {
                    old.end_reopen(None);
                    return Err(CliFailure::new(detail));
                }
                break;
            }
            if Instant::now() >= until {
                // Cancellation and actor settlement share one ticket lock.
                // If settlement won, join it normally; otherwise the stale
                // fence cannot quit and the same actor remains served.
                if let Some(result) = old.cancel_reopen(ticket) {
                    if let Err(detail) = result {
                        old.end_reopen(None);
                        return Err(CliFailure::new(detail));
                    }
                    break;
                }
                old.end_reopen(None);
                return Err(CliFailure::new(
                    "the session did not acknowledge its release in time; the fence was cancelled",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.busy.store(BUSY_RELEASING, Ordering::Release);
        crate::test_support_runner::reopen_cut("fenced");
        let source = old.session_id();
        let Slot::Resident { worker, .. } = std::mem::replace(
            &mut *slot,
            Slot::Dormant {
                session_id: source.clone(),
            },
        ) else {
            unreachable!("the slot lock retains the resident worker");
        };
        if let Err(failure) = join(worker, self.release_join) {
            old.end_reopen(None);
            self.busy.store(BUSY_NONE, Ordering::Release);
            return Err(failure);
        }
        crate::test_support_runner::reopen_cut("released");
        let changed = std::thread::scope(|scope| scope.spawn(|| change(&source)).join())
            .unwrap_or_else(|_| Err(CliFailure::new("the binding repair panicked")));
        crate::test_support_runner::reopen_cut("carried");
        let (target, mut root) = changed
            .as_ref()
            .cloned()
            .unwrap_or_else(|_| (source, old.root()));
        self.set_known(&target);
        *slot = Slot::Dormant {
            session_id: target.clone(),
        };
        self.busy.store(BUSY_OPENING, Ordering::Release);
        let opened = self.open_on_thread(&target);
        let opening = opened.map(|opened| {
            root = opened.root.clone();
            self.set_known(&opened.session_id);
            *slot = resident(opened);
        });
        crate::test_support_runner::reopen_cut("opened");
        old.end_reopen(Some((&root, attachment, request)));
        self.busy.store(BUSY_NONE, Ordering::Release);
        changed.and(opening).map(|()| root)
    }

    fn end(&self, deadline: Duration, ask: fn(&WireSession<P>)) -> CliResult<()> {
        let mut slot = self.lock();
        if let Slot::Resident { session, .. } = &*slot {
            ask(session);
        }
        let id = match &*slot {
            Slot::Resident { session, .. } => session.session_id(),
            Slot::Dormant { session_id } => session_id.clone(),
        };
        self.set_known(&id);
        match std::mem::replace(&mut *slot, Slot::Dormant { session_id: id }) {
            Slot::Resident { worker, .. } => {
                self.busy.store(BUSY_RELEASING, Ordering::Release);
                let joined = join(worker, deadline);
                self.busy.store(BUSY_NONE, Ordering::Release);
                joined
            }
            Slot::Dormant { .. } => Ok(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Slot<P>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn open_on_thread(&self, session: &str) -> CliResult<OpenedSession<P>> {
        std::thread::scope(|scope| scope.spawn(|| (self.open)(session)).join())
            .map_err(|_| CliFailure::new("the session's open panicked"))?
    }
}

fn resident<P>(opened: OpenedSession<P>) -> Slot<P>
where
    P: ProtocolExtension + 'static,
    P::Intent: Send,
    P::Observation: Send,
{
    let roots = opened.roots;
    Slot::Resident {
        session: Arc::new(
            WireSession::start_counted(
                opened.root,
                opened.session_id,
                opened.actions,
                opened.events,
                move |id| roots(id),
                &opened.attachments,
            )
            .with_runner_pending(opened.runner_pending),
        ),
        worker: opened.worker,
        attachments: opened.attachments,
    }
}

/// Moves a session that released itself, or is releasing, to dormant, and
/// joins its worker within `deadline`.
fn settle<P: ProtocolExtension>(slot: &mut Slot<P>, deadline: Duration) -> CliResult<()> {
    let Slot::Resident {
        session,
        attachments,
        ..
    } = &*slot
    else {
        return Ok(());
    };
    if !session.ended() && !attachments.releasing() {
        return Ok(());
    }
    let session_id = session.session_id();
    let Slot::Resident { worker, .. } = std::mem::replace(slot, Slot::Dormant { session_id })
    else {
        return Ok(());
    };
    join(worker, deadline)
}

fn join(worker: HostWorkerHandle, deadline: Duration) -> CliResult<()> {
    let until = Instant::now() + deadline;
    while !worker.is_finished() {
        if Instant::now() >= until {
            return Err(CliFailure::new(
                "the session's host worker did not end in time",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    worker
        .join()
        .map_err(|_| CliFailure::new("the session's host worker failed"))?
        .map(|_| ())
}
