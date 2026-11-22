use plakous_host_protocol::wire::RequestId;
use tokio::sync::oneshot::{self, error::TryRecvError};

use super::super::slot::HeldSession;
use super::archived::StoredAttachment;
use super::{
    Arc, CliFailure, CliResult, ClientFrame, DeserializeOwned, Forwarded, HostConnection,
    HostFrame, NodeAddress, Outcome, PoisonError, ProtocolExtension, RequestRefusal, Serialize,
    SessionHost, Write, stopping, write_frame,
};

const MAX_QUEUED_ATTACHES: usize = 4;

enum Target<P: ProtocolExtension> {
    Resident(Undelivered<P>),
    Stored(StoredAttachment<P>),
    Refused(RequestRefusal),
}

struct Undelivered<P: ProtocolExtension> {
    held: Option<HeldSession<P>>,
    from_open: bool,
}

impl<P: ProtocolExtension> Drop for Undelivered<P> {
    fn drop(&mut self) {
        if self.from_open
            && let Some((session, _, _)) = &self.held
        {
            // Unlike ReleaseSession this cannot detach a different client
            // that attached while this request was being cancelled.
            session.release_unattached();
        }
    }
}

pub(super) struct AttachRequest {
    id: RequestId,
    node: NodeAddress,
    text: String,
    allow_open: bool,
}

pub(super) struct PendingAttach<P: ProtocolExtension> {
    request: AttachRequest,
    result: oneshot::Receiver<CliResult<Target<P>>>,
    runtime: tokio::runtime::Handle,
    retry: bool,
}

impl<P: ProtocolExtension> SessionHost<P>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    fn prepare_attach(&self, node: &NodeAddress, allow_open: bool) -> CliResult<Target<P>> {
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
            return Ok(Target::Resident(Undelivered {
                held: Some(resident),
                from_open: false,
            }));
        }
        if !allow_open {
            return Ok(Target::Refused(RequestRefusal::Backpressure));
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
            Ok((resident, from_open)) => {
                let opened = Undelivered {
                    held: Some(resident),
                    from_open,
                };
                if self.closed() {
                    return Ok(Target::Refused(stopping()));
                }
                Ok(Target::Resident(opened))
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
    pub(super) fn attach_in_flight(&self, id: RequestId) -> bool {
        self.attaching
            .as_ref()
            .is_some_and(|attach| attach.request.id == id)
            || self.attach_queue.iter().any(|attach| attach.id == id)
    }

    pub(super) fn start_attach(
        &mut self,
        id: RequestId,
        node: NodeAddress,
        text: &str,
        allow_open: bool,
    ) -> CliResult<Forwarded<P>> {
        if self.host_reads.contains_key(&id) {
            return Ok(Forwarded::Answered(Outcome::Refused(
                RequestRefusal::Malformed {
                    detail: "request id is already in flight".into(),
                },
            )));
        }
        if self.attaching.is_some() {
            if self.attach_queue.len() >= MAX_QUEUED_ATTACHES {
                return Ok(Forwarded::Answered(Outcome::Refused(
                    RequestRefusal::Backpressure,
                )));
            }
            self.attach_queue.push_back(AttachRequest {
                id,
                node,
                text: text.to_owned(),
                allow_open,
            });
            return Ok(Forwarded::Reading(true));
        }
        let Some(runtime) = tokio::runtime::Handle::try_current()
            .ok()
            .filter(|_| self.buffered_output)
        else {
            let target = self.host.prepare_attach(&node, allow_open)?;
            if let Some(done) = self.finish_attach(target, text, true)? {
                return Ok(done);
            }
            let target = self.host.prepare_attach(&node, allow_open)?;
            let Some(done) = self.finish_attach(target, text, false)? else {
                unreachable!("the second preparation answers");
            };
            return Ok(done);
        };
        self.prepare_buffered(
            AttachRequest {
                id,
                node,
                text: text.to_owned(),
                allow_open,
            },
            runtime,
            true,
        );
        Ok(Forwarded::Reading(true))
    }

    fn prepare_buffered(
        &mut self,
        request: AttachRequest,
        runtime: tokio::runtime::Handle,
        retry: bool,
    ) {
        let (send, result) = oneshot::channel();
        let host = Arc::clone(&self.host);
        let wake = Arc::clone(&self.read_ready);
        let node = request.node.clone();
        let allow_open = request.allow_open;
        runtime.spawn_blocking(move || {
            let paused = host
                .upgrade_admission
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if send.is_closed() {
                return;
            }
            let result = host.prepare_attach(&node, allow_open && !*paused);
            if send.is_closed() {
                return;
            }
            let _ = send.send(result);
            wake.notify.notify_one();
        });
        self.attaching = Some(PendingAttach {
            request,
            result,
            runtime,
            retry,
        });
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
        let _timing = super::super::test_timing::phase("settle attach").entered();
        let Some(PendingAttach {
            request,
            runtime,
            retry,
            ..
        }) = self.attaching.take()
        else {
            unreachable!("the connection owns its pending attach");
        };
        let Some(finished) = self.finish_attach(target, &request.text, retry)? else {
            self.prepare_buffered(request, runtime, false);
            return Ok(());
        };
        self.complete_attach(request.id, finished)?;
        while let Some(AttachRequest {
            id,
            node,
            text,
            allow_open,
        }) = self.attach_queue.pop_front()
        {
            let started = self.start_attach(id, node, &text, allow_open)?;
            self.complete_attach(id, started)?;
            if self.attaching.is_some() {
                break;
            }
        }
        Ok(())
    }

    fn complete_attach(&mut self, id: RequestId, result: Forwarded<P>) -> CliResult<()> {
        if let Forwarded::Answered(outcome) = result {
            write_frame(&mut self.out, &HostFrame::<P>::Response { id, outcome })?;
        }
        Ok(())
    }

    // None asks for one fresh preparation without yielding the current
    // request's place to another queued attach.
    fn finish_attach(
        &mut self,
        target: Target<P>,
        text: &str,
        retry: bool,
    ) -> CliResult<Option<Forwarded<P>>> {
        match target {
            Target::Resident(mut resident) => {
                if self.host.closed() {
                    return Ok(Some(Forwarded::Answered(Outcome::Refused(stopping()))));
                }
                let Some((session, _, releases)) = &resident.held else {
                    unreachable!("a prepared attach owns its session hold");
                };
                if session.ended() || session.releases() != *releases {
                    if retry && session.lifecycle_retired() {
                        return Ok(None);
                    }
                    return Ok(Some(Forwarded::Answered(Outcome::Refused(
                        RequestRefusal::SessionUnavailable {
                            detail: "the session released during attach".into(),
                        },
                    ))));
                }
                let Some(held) = resident.held.take() else {
                    unreachable!("a prepared attach owns its session hold");
                };
                self.attach_resident(held, text).map(Some)
            }
            Target::Stored(stored) => self.attach_stored(stored, text).map(Some),
            Target::Refused(refusal) => Ok(Some(Forwarded::Answered(Outcome::Refused(refusal)))),
        }
    }
}
