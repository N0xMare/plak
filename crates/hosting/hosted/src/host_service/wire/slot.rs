//! One session a host serves, resident or dormant (spec 02 §4.4). A
//! resident session is its host worker and the [`WireSession`] its
//! connections share. The session releases itself once nothing keeps it
//! resident (its actor's idle release); the slot then joins the worker and
//! holds only the session's id. The next trigger, a connection that wants
//! the session, materializes it again through the exact reopen and serves
//! it as before. A trigger that arrives while the session is releasing
//! waits for the release to commit, then opens it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::NodeAddress;
use plakous_host_protocol::{IntentSender, ObservationReceiver, ProtocolExtension};

use super::super::AttachmentCount;
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
    },
    Dormant {
        session_id: String,
    },
}

/// One session a host serves, resident or dormant.
pub struct SessionSlot<P: ProtocolExtension> {
    slot: Mutex<Slot<P>>,
    open: Opener<P>,
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
        }
    }

    /// The resident session: the one served now, or, when it released
    /// itself, the same session (or the one it last switched to) opened
    /// again. Blocks while a release joins or an open runs.
    ///
    /// # Errors
    ///
    /// The release did not join in time, or the session does not open.
    pub fn resident(&self) -> CliResult<Arc<WireSession<P>>> {
        let mut slot = self.lock();
        settle(&mut slot)?;
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
        match &*slot {
            Slot::Resident { session, .. } => Ok(Arc::clone(session)),
            Slot::Dormant { .. } => Err(CliFailure::new("the session did not open")),
        }
    }

    /// Whether the session is dormant now: released, or releasing.
    #[must_use]
    pub fn dormant(&self) -> bool {
        match &*self.lock() {
            Slot::Resident { session, .. } => session.ended(),
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
    }
}

/// Moves a session whose service ended (it released itself) to dormant,
/// once its worker joined.
fn settle<P: ProtocolExtension>(slot: &mut Slot<P>) -> CliResult<()> {
    let Slot::Resident { session, .. } = &*slot else {
        return Ok(());
    };
    if !session.ended() {
        return Ok(());
    }
    let session_id = session.session_id();
    let Slot::Resident { worker, .. } = std::mem::replace(slot, Slot::Dormant { session_id })
    else {
        return Ok(());
    };
    join(worker, RELEASE_JOIN)
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
