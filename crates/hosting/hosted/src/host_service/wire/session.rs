//! One session's wire side shared by many connections: its host service,
//! fed by a pump thread that is the session's only observation consumer,
//! and a change signal the connections wait on. A connection that leaves
//! detaches; the session runs until it ends on its own. When the session
//! switches to another (`/new`, `/resume`, a rebind), every attachment ends
//! with `moved` naming the new session's root and the service serves that
//! session; a later connection is welcomed to it.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use plakous_host_protocol::wire::{ClientFrame, HostFrame, NodeAddress};
use plakous_host_protocol::{IntentSender, ObservationReceiver, ProtocolExtension, SessionIntent};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::watch;

use super::super::{HostService, TreeNodes};
use super::served::{self, ServedSession, Shared};
use super::{ATTACHMENT_CAPACITY, Face, Leave, LostTranscripts, POLL, WireConnection};

/// A session served to any number of connections over time.
pub struct WireSession<P: ProtocolExtension> {
    actions: IntentSender<P>,
    served: Shared<P>,
    changes: watch::Receiver<u64>,
    ended: Arc<AtomicBool>,
}

impl<P: ProtocolExtension> WireSession<P> {
    /// Serves the session `session_id` that `actions` sends to and `events`
    /// observes, whose root node is `root`, starting its pump; `roots` finds
    /// the root of a session it switches to. A switch whose root cannot be
    /// found ends the service as the session's end does.
    #[must_use]
    pub fn start(
        root: NodeAddress,
        session_id: String,
        actions: IntentSender<P>,
        events: ObservationReceiver<P>,
        roots: impl Fn(&str) -> Option<NodeAddress> + Send + Sync + 'static,
    ) -> Self
    where
        P: 'static,
        P::Intent: Send,
        P::Observation: Send,
    {
        let service = HostService::new(actions.clone(), root.clone(), ATTACHMENT_CAPACITY)
            .with_nodes(Arc::new(TreeNodes::new(root.tree_id)));
        let served = ServedSession::shared(session_id, Box::new(roots), service);
        let (notify, changes) = watch::channel(0_u64);
        let ended = Arc::new(AtomicBool::new(false));
        let pumped = Arc::clone(&served);
        let pump_ended = Arc::clone(&ended);
        // The pump is the session's only observation consumer; it ends
        // with the session's observations and is never joined.
        thread::spawn(move || {
            let mut lost = LostTranscripts::default();
            loop {
                match events.recv_timeout(POLL) {
                    Ok(event) => {
                        let mut guard = served::lock(&pumped);
                        let mut found = guard.observe(event);
                        while let Ok(event) = events.try_recv() {
                            found &= guard.observe(event);
                        }
                        lost.check(&guard.service(), &events);
                        drop(guard);
                        if !found {
                            pump_ended.store(true, Ordering::Release);
                        }
                        notify.send_modify(|revision| *revision = revision.wrapping_add(1));
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        served::lock(&pumped).end();
                        pump_ended.store(true, Ordering::Release);
                        notify.send_modify(|revision| *revision = revision.wrapping_add(1));
                        return;
                    }
                }
            }
        });
        Self {
            actions,
            served,
            changes,
            ended,
        }
    }

    /// The served session's root node: the opened session's, or the one it
    /// last switched to.
    #[must_use]
    pub fn root(&self) -> NodeAddress {
        served::lock(&self.served).root().clone()
    }

    /// The served session: the opened one, or the one it last switched to.
    #[must_use]
    pub fn session_id(&self) -> String {
        served::lock(&self.served).session().to_owned()
    }

    /// A new connection writing its frames to `out`, serving this session
    /// to a client its acceptor marked `face`; it detaches when it leaves,
    /// and ends once `stopped` turns true. Its welcome names
    /// [`WireConnection::welcomed`], taken from the same generation as its
    /// service handle.
    pub fn connect<W: Write>(
        &self,
        out: W,
        face: Face,
        stopped: impl Fn() -> bool + Send + Sync + 'static,
    ) -> WireConnection<P, W>
    where
        HostFrame<P>: Serialize,
        ClientFrame<P>: DeserializeOwned,
    {
        let guard = served::lock(&self.served);
        let (service, generation, session) = (
            guard.service(),
            guard.generation(),
            guard.session().to_owned(),
        );
        drop(guard);
        WireConnection::on_service(service, out, self.actions.clone(), Leave::Detach)
            .face(face)
            .with_stop(stopped)
            .on_served(Arc::clone(&self.served), generation, session)
    }

    /// Changes whenever the session published something or ended.
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.clone()
    }

    /// Whether the service ended: the session closed, or switched to one
    /// whose root cannot be found.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Asks the session to quit; it ends once it has.
    pub fn quit(&self) {
        let _ = self.actions.send(SessionIntent::Quit);
    }
}
