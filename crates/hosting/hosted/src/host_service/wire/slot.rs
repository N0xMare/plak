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

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::NodeAddress;
use plakous_host_protocol::{IntentSender, ObservationReceiver, ProtocolExtension};

use super::super::{AttachmentCount, ResidencyHold};
use super::WireSession;
use crate::error::{CliFailure, CliResult};
use crate::runtime::HostWorkerHandle;

/// How long a release may take to join before the slot gives up on it.
const RELEASE_JOIN: Duration = Duration::from_secs(60);

/// A session opened on a host worker: what the slot serves it with.
pub struct OpenedSession<P: ProtocolExtension> {
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
        Self {
            slot: Mutex::new(resident(opened)),
            open: Box::new(open),
            release_join: RELEASE_JOIN,
        }
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
        let mut slot = self.lock();
        loop {
            settle(&mut slot, self.release_join)?;
            if let Slot::Dormant { session_id } = &*slot {
                // On a thread of its own: an open builds the session's own
                // executor, which a caller's async runtime would refuse.
                let open = &self.open;
                let opened = std::thread::scope(|scope| {
                    scope
                        .spawn(|| open(session_id))
                        .join()
                        .map_err(|_| CliFailure::new("the session's open panicked"))
                })??;
                *slot = resident(opened);
            }
            if let Slot::Resident {
                session,
                attachments,
                ..
            } = &*slot
                && let Some(hold) = attachments.hold()
            {
                return Ok((Arc::clone(session), hold));
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

    /// Quits a resident session and joins its worker within `deadline`; a
    /// dormant one has nothing to quit.
    ///
    /// # Errors
    ///
    /// The worker did not end in time, or ended with a failure.
    pub fn quit(&self, deadline: Duration) -> CliResult<()> {
        let mut slot = self.lock();
        if let Slot::Resident { session, .. } = &*slot {
            session.quit();
        }
        let id = match &*slot {
            Slot::Resident { session, .. } => session.session_id(),
            Slot::Dormant { session_id } => session_id.clone(),
        };
        match std::mem::replace(&mut *slot, Slot::Dormant { session_id: id }) {
            Slot::Resident { worker, .. } => join(worker, deadline),
            Slot::Dormant { .. } => Ok(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Slot<P>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
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
        session: Arc::new(WireSession::start_counted(
            opened.root,
            opened.session_id,
            opened.actions,
            opened.events,
            move |id| roots(id),
            &opened.attachments,
        )),
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
