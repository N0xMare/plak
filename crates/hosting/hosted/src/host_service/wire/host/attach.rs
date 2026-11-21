use plakous_host_protocol::wire::RequestId;
use tokio::sync::oneshot::{self, error::TryRecvError};

use super::super::slot::HeldSession;
use super::archived::StoredAttachment;
use super::{
    Arc, CliFailure, CliResult, ClientFrame, DeserializeOwned, Forwarded, HostConnection,
    HostFrame, NodeAddress, Outcome, PoisonError, ProtocolExtension, RequestRefusal, Serialize,
    SessionHost, Write, stopping, write_frame,
};

enum Target<P: ProtocolExtension> {
    Resident(HeldSession<P>),
    Stored(StoredAttachment<P>),
    Refused(RequestRefusal),
}

pub(super) struct PendingAttach<P: ProtocolExtension> {
    id: RequestId,
    text: String,
    result: oneshot::Receiver<CliResult<Target<P>>>,
}

impl<P: ProtocolExtension> PendingAttach<P> {
    pub(super) const fn id(&self) -> RequestId {
        self.id
    }
}

impl<P: ProtocolExtension> SessionHost<P>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    fn prepare_attach(&self, node: &NodeAddress) -> CliResult<Target<P>> {
        if let Some(successor) = self
            .sessions
            .successor_node(node)
            .map_err(CliFailure::new)?
        {
            return Ok(Target::Refused(RequestRefusal::Moved { node: successor }));
        }
        if self.closed() {
            return Ok(Target::Refused(stopping()));
        }
        let slot = match self.resolve(node) {
            Ok(slot) => slot,
            Err(refusal) => return Ok(Target::Refused(refusal)),
        };
        // Archiving releases the live slot before publishing the archive
        // flag. A held resident cannot have an archived transcript.
        if let Some(resident) = slot.try_hold_resident() {
            return Ok(Target::Resident(resident));
        }
        let id = match slot.peek() {
            super::SlotView::Resident(session) => session.session_id(),
            super::SlotView::Dormant(id)
            | super::SlotView::Opening(id)
            | super::SlotView::Releasing(id) => id,
        };
        let cached = slot
            .startup_retrying()
            .then(|| slot.open_failure())
            .flatten();
        if let Some(target) = self.stored_target(node, &id, cached.as_ref()) {
            return Ok(target);
        }
        match slot.resident_at_handoff() {
            Ok(resident) => {
                self.sessions.settle_directory();
                Ok(Target::Resident(resident))
            }
            Err(failure) => Ok(self
                .stored_target(node, &id, Some(&failure))
                .unwrap_or_else(|| Target::Refused(self.sessions.refusal(&id, &failure)))),
        }
    }

    fn stored_target(
        &self,
        node: &NodeAddress,
        id: &str,
        failure: Option<&CliFailure>,
    ) -> Option<Target<P>> {
        // Ownership refusals must be rechecked when their holder goes away.
        let failure = failure.filter(|failure| failure.code() != Some("tree_hosted_elsewhere"));
        let view = match self.sessions.stored_view(id, failure.is_some()) {
            Ok(view) => view?,
            Err(refusal) => return Some(Target::Refused(refusal)),
        };
        let refusal = if view.archived {
            RequestRefusal::SessionArchived {
                session: id.to_owned(),
            }
        } else {
            self.sessions.refusal(id, failure?)
        };
        if !matches!(
            refusal,
            RequestRefusal::SessionArchived { .. }
                | RequestRefusal::SessionUnavailable { .. }
                | RequestRefusal::PlacementRefused { .. }
                | RequestRefusal::BindingRefused { .. }
        ) {
            return None;
        }
        if view.root != *node {
            return Some(Target::Refused(if view.archived {
                RequestRefusal::UnknownNode
            } else {
                refusal
            }));
        }
        Some(Target::Stored(StoredAttachment::new(
            view, refusal, failure,
        )))
    }
}

impl<P: ProtocolExtension, W: Write + Clone> HostConnection<P, W>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    pub(super) fn start_attach(
        &mut self,
        id: RequestId,
        node: NodeAddress,
        text: &str,
    ) -> CliResult<Forwarded<P>> {
        if self.host_reads.contains_key(&id) {
            return Ok(Forwarded::Answered(Outcome::Refused(
                RequestRefusal::Malformed {
                    detail: "request id is already in flight".into(),
                },
            )));
        }
        if self.attaching.is_some() {
            return Ok(Forwarded::Answered(Outcome::Refused(
                RequestRefusal::OneInFlight,
            )));
        }
        let Some(runtime) = tokio::runtime::Handle::try_current()
            .ok()
            .filter(|_| self.buffered_output)
        else {
            let target = self.host.prepare_attach(&node)?;
            return self.finish_attach(target, text);
        };
        let (send, result) = oneshot::channel();
        let host = Arc::clone(&self.host);
        let wake = Arc::clone(&self.read_ready);
        self.attaching = Some(PendingAttach {
            id,
            text: text.to_owned(),
            result,
        });
        runtime.spawn_blocking(move || {
            let paused = host
                .upgrade_admission
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if send.is_closed() {
                return;
            }
            let result = if *paused {
                Ok(Target::Refused(RequestRefusal::SessionUnavailable {
                    detail: "the host is preparing an upgrade; retry after it finishes".into(),
                }))
            } else {
                host.prepare_attach(&node)
            };
            let _ = send.send(result);
            wake.notify.notify_one();
        });
        Ok(Forwarded::Reading(true))
    }

    pub(super) fn flush_attach(&mut self) -> CliResult<()> {
        let Some(attach) = &mut self.attaching else {
            return Ok(());
        };
        let target = match attach.result.try_recv() {
            Ok(result) => result?,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Closed) => return Err(CliFailure::new("attach worker ended")),
        };
        let Some(PendingAttach { id, text, .. }) = self.attaching.take() else {
            unreachable!("the connection owns its pending attach");
        };
        if let Forwarded::Answered(outcome) = self.finish_attach(target, &text)? {
            write_frame(&mut self.out, &HostFrame::<P>::Response { id, outcome })?;
        }
        Ok(())
    }

    fn finish_attach(&mut self, target: Target<P>, text: &str) -> CliResult<Forwarded<P>> {
        match target {
            Target::Resident(resident) => self.attach_resident(resident, text),
            Target::Stored(stored) => self.attach_stored(stored, text),
            Target::Refused(refusal) => Ok(Forwarded::Answered(Outcome::Refused(refusal))),
        }
    }
}
