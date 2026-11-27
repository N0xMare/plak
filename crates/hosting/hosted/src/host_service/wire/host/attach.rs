use std::time::{Duration, Instant};

use plakous_host_protocol::wire::RequestId;
use tokio::sync::oneshot::{self, error::TryRecvError};

use super::super::slot::HeldSession;
use super::archived::StoredAttachment;
use super::{
    Arc, CliFailure, CliResult, ClientFrame, DeserializeOwned, Forwarded, HostConnection,
    HostFrame, NodeAddress, NodeLink, Outcome, PreparedViewport, ProtocolExtension, RequestRefusal,
    Serialize, SessionHost, ViewportMode, ViewportRead, ViewportRevision, Write, stopping,
};

const MAX_QUEUED_ATTACHES: usize = 4;
const VIEWPORT_CAPTURE_WAIT: Duration = Duration::from_millis(120);

enum Target<P: ProtocolExtension> {
    Resident(
        Undelivered<P>,
        Option<(ViewportRead, Box<dyn PreparedViewport>)>,
    ),
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
    retained: Option<ViewportRevision>,
}

pub(super) struct PendingAttach<P: ProtocolExtension> {
    request: AttachRequest,
    result: oneshot::Receiver<Target<P>>,
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
    fn prepare_attach(
        &self,
        node: &NodeAddress,
        allow_open: bool,
        retained: Option<&ViewportRevision>,
    ) -> Target<P> {
        if self.closed() {
            return Target::Refused(stopping());
        }
        let slot = match self.resolve(node) {
            Ok(slot) => slot,
            Err(refusal) => return Target::Refused(refusal),
        };
        // Archiving releases the live slot before publishing the archive
        // flag. A held resident cannot have an archived transcript.
        if let Some(resident) = slot.try_hold_resident() {
            return self.prepare_resident(
                node,
                Undelivered {
                    held: Some(resident),
                    from_open: false,
                },
                retained,
            );
        }
        if !allow_open {
            return Target::Refused(RequestRefusal::Backpressure);
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
            return target;
        }
        // A start that has not finished its inventories serves saved history
        // without touching the slot, so no open failure is recorded.
        if let Some(deferred) = self.sessions.deferred_open(&id) {
            return match self.stored_target(node, &id, Some(&deferred)) {
                Some(Target::Stored(stored)) => Target::Stored(stored.deferred(self.defer(&id))),
                Some(target) => target,
                None => Target::Refused(self.sessions.refusal(&id, &deferred)),
            };
        }
        match slot.resident_at_handoff() {
            Ok((resident, from_open)) => {
                let opened = Undelivered {
                    held: Some(resident),
                    from_open,
                };
                if self.closed() {
                    return Target::Refused(stopping());
                }
                self.prepare_resident(node, opened, retained)
            }
            Err(failure) => self
                .stored_target(node, &id, Some(&failure))
                .unwrap_or_else(|| Target::Refused(self.sessions.refusal(&id, &failure))),
        }
    }

    /// Records a read-only link whose open was deferred; the link holds
    /// the returned token.
    fn defer(&self, session: &str) -> Arc<()> {
        let token = Arc::new(());
        let mut deferred = self
            .deferred
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        deferred.retain(|(_, link)| link.strong_count() > 0);
        deferred.push((session.to_owned(), Arc::downgrade(&token)));
        token
    }

    /// Opens, on a thread of its own, each session a live read-only link
    /// holds because its open was deferred. Each open moves its links live,
    /// as any background open does; a failed open leaves them read-only.
    pub fn open_deferred(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let mut sessions = std::mem::take(
            &mut *self
                .deferred
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_iter()
        .filter(|(_, link)| link.strong_count() > 0)
        .map(|(session, _)| session)
        .collect::<Vec<_>>();
        sessions.sort();
        sessions.dedup();
        let host = Arc::clone(self);
        std::thread::spawn(move || {
            for session in sessions {
                if host.closed() {
                    return;
                }
                let _ = host.slot(&session).resident();
                host.changed();
            }
        })
    }

    fn prepare_resident(
        &self,
        node: &NodeAddress,
        resident: Undelivered<P>,
        retained: Option<&ViewportRevision>,
    ) -> Target<P> {
        let Some((session, _, _)) = &resident.held else {
            unreachable!("a prepared attach owns its session hold");
        };
        let Some(root) = &session.root_transcript else {
            return Target::Resident(resident, None);
        };
        if session.root() != *node {
            return Target::Resident(resident, None);
        }
        let deadline = Instant::now() + VIEWPORT_CAPTURE_WAIT;
        let id = session.session_id();
        loop {
            if let Some(stamp) = session.viewport_read(node, self.generation) {
                match self
                    .sessions
                    .prepare_viewport(&id, self.generation, Some(root), retained)
                {
                    Ok(Some(page)) => return Target::Resident(resident, Some((stamp, page))),
                    Ok(None) => return Target::Resident(resident, None),
                    Err(RequestRefusal::Backpressure) => {}
                    Err(refusal) => return Target::Refused(refusal),
                }
            }
            // Store and observation publication run independently. Let an
            // opening or committing root reach its next coherent cut.
            if Instant::now() >= deadline || session.ended() {
                return Target::Refused(RequestRefusal::Backpressure);
            }
            std::thread::sleep(Duration::from_millis(4));
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
        let view = match self.sessions.stored_view(id, failure.is_some(), false) {
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
        let page = match self
            .sessions
            .prepare_viewport(id, self.generation, None, None)
        {
            Ok(page) => page,
            Err(reason) => return Some(Target::Refused(reason)),
        };
        // Only a product without a paged viewport needs the whole saved
        // presentation.
        let view = if page.is_none() {
            match self.sessions.stored_view(id, failure.is_some(), true) {
                Ok(Some(view)) => view,
                Ok(None) => return None,
                Err(refusal) => return Some(Target::Refused(refusal)),
            }
        } else {
            view
        };
        Some(Target::Stored(StoredAttachment::new(
            view, refusal, failure, page,
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
        retained: Option<ViewportRevision>,
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
                retained,
            });
            return Ok(Forwarded::Reading(true));
        }
        let Some(runtime) = tokio::runtime::Handle::try_current()
            .ok()
            .filter(|_| self.buffered_output)
        else {
            let target = self
                .host
                .prepare_attach(&node, allow_open, retained.as_ref());
            if let Some(done) = self.finish_attach(id, target, text, true)? {
                return Ok(done);
            }
            let target = self.host.prepare_attach(&node, allow_open, None);
            let Some(done) = self.finish_attach(id, target, text, false)? else {
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
                retained,
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
        let retained = request.retained.clone();
        runtime.spawn_blocking(move || {
            if send.is_closed() {
                return;
            }
            let result = host.prepare_attach(&node, allow_open, retained.as_ref());
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
            Ok(result) => result,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Closed) => return Err(CliFailure::new("attach worker ended")),
        };
        let _timing = super::super::test_timing::phase("settle attach").entered();
        let Some(PendingAttach {
            mut request,
            runtime,
            retry,
            ..
        }) = self.attaching.take()
        else {
            unreachable!("the connection owns its pending attach");
        };
        let Some(finished) = self.finish_attach(request.id, target, &request.text, retry)? else {
            request.retained = None;
            self.prepare_buffered(request, runtime, false);
            return Ok(());
        };
        self.complete_attach(request.id, finished)?;
        while let Some(AttachRequest {
            id,
            node,
            text,
            allow_open,
            retained,
        }) = self.attach_queue.pop_front()
        {
            let started = self.start_attach(id, node, &text, allow_open, retained)?;
            self.complete_attach(id, started)?;
            if self.attaching.is_some() {
                break;
            }
        }
        Ok(())
    }

    fn complete_attach(&mut self, id: RequestId, result: Forwarded<P>) -> CliResult<()> {
        if let Forwarded::Answered(outcome) = result {
            self.write(&HostFrame::<P>::Response { id, outcome })?;
        }
        Ok(())
    }

    // None asks for one fresh preparation without yielding the current
    // request's place to another queued attach.
    fn finish_attach(
        &mut self,
        id: RequestId,
        target: Target<P>,
        text: &str,
        retry: bool,
    ) -> CliResult<Option<Forwarded<P>>> {
        match target {
            Target::Resident(mut resident, page) => {
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
                if let Some((stamp, page)) = page {
                    if let Some(link) = self.node.as_mut().filter(|link| {
                        Arc::ptr_eq(&held.0, &link.session) && link.releases == held.2
                    }) {
                        return Self::write_viewport(link, id, &stamp, page, retry);
                    }
                    let mut link = self.resident_link(held);
                    let result = Self::write_viewport(&mut link, id, &stamp, page, retry)?;
                    if matches!(result, Some(Forwarded::Reading(true))) {
                        self.unlink();
                        self.node = Some(link);
                    }
                    return Ok(result);
                }
                self.attach_resident(held, text).map(Some)
            }
            Target::Stored(stored) => self.attach_stored(id, stored, text).map(Some),
            Target::Refused(refusal) => Ok(Some(Forwarded::Answered(Outcome::Refused(refusal)))),
        }
    }

    fn write_viewport(
        link: &mut NodeLink<P, W>,
        id: RequestId,
        stamp: &ViewportRead,
        page: Box<dyn PreparedViewport>,
        retry: bool,
    ) -> CliResult<Option<Forwarded<P>>> {
        let connection = &mut link.connection;
        let start = match page.mode() {
            ViewportMode::Replay(revision) => {
                connection.replay_viewport(stamp, page.fence(), &revision)
            }
            ViewportMode::Page(revision) => {
                connection.install_viewport_page(stamp, page.fence(), revision)
            }
        };
        match start {
            Ok(Some(start)) => {
                let attachment = start.attachment;
                let mut binding = None;
                connection.write_viewport_prefix(attachment, |out| {
                    binding = Some(page.write(out, id, start)?);
                    Ok(())
                })?;
                connection.history = binding.map(|binding| (attachment, binding));
                Ok(Some(Forwarded::Reading(true)))
            }
            Ok(None) if retry => Ok(None),
            Ok(None) => Ok(Some(Forwarded::Answered(Outcome::Refused(
                RequestRefusal::Backpressure,
            )))),
            Err(reason) => Ok(Some(Forwarded::Answered(Outcome::Refused(reason)))),
        }
    }
}
