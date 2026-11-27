//! Stored attachments own only a persisted view, never a runtime or lease.

use plakous_host_protocol::{
    SessionObservation,
    status::{HostPhase, HostStatusDetail},
    views::transcript::{NoticeScope, TranscriptItem, TranscriptRow},
};

use super::{
    Arc, CliResult, ClientFrame, DeserializeOwned, Forwarded, HostConnection, HostFrame,
    ProtocolExtension, ReadOnlyLink, RequestRefusal, Serialize, StoredSessionView, WireConnection,
    Write,
};

pub(super) struct StoredAttachment<P: ProtocolExtension> {
    session: String,
    refusal: RequestRefusal,
    service: crate::host_service::HostService<P>,
    actions: plakous_host_protocol::IntentSender<P>,
    mailbox: plakous_host_protocol::IntentReceiver<P>,
    page: Option<Box<dyn super::PreparedViewport>>,
    deferral: Option<Arc<()>>,
}

impl<P: ProtocolExtension> StoredAttachment<P> {
    pub(super) fn new(
        mut view: StoredSessionView<P>,
        refusal: RequestRefusal,
        failure: Option<&crate::error::CliFailure>,
        page: Option<Box<dyn super::PreparedViewport>>,
    ) -> Self {
        if let Some(failure) = failure.filter(|_| !view.archived) {
            view.mark_unavailable(failure);
        }
        let (actions, mailbox) = plakous_host_protocol::intent_channel();
        let service = crate::host_service::HostService::new(
            actions.clone(),
            view.root,
            super::super::ATTACHMENT_CAPACITY,
        );
        for observation in view.observations {
            if page.is_none() || !matches!(observation, SessionObservation::PresentationSnapshot(_))
            {
                service.observe(observation);
            }
        }
        Self {
            session: view.session,
            refusal,
            service,
            actions,
            mailbox,
            page,
            deferral: None,
        }
    }

    /// Marks a saved view served because its open was deferred.
    pub(super) fn deferred(mut self, token: Arc<()>) -> Self {
        self.deferral = Some(token);
        self
    }
}

impl<P: ProtocolExtension> StoredSessionView<P> {
    fn mark_unavailable(&mut self, failure: &crate::error::CliFailure) {
        let mut status = failure.host_failure();
        status.diagnostic = format!(
            "Saved history only; session unavailable: {}. This attachment is read-only; \
             historical activity is not live status.",
            failure.message()
        )
        .into();
        for observation in &mut self.observations {
            if let SessionObservation::PresentationSnapshot(snapshot) = observation {
                // Display-only: never write a failed attachment into saved history.
                snapshot.items.push(TranscriptItem {
                    key: crate::sessions::provisional_key(),
                    row: TranscriptRow::warning(NoticeScope::Session, status.diagnostic.as_str())
                        .with_optional_remedy(status.remedy.as_ref()),
                });
                snapshot.items = crate::runtime::logging::project_keyed_snapshot(std::mem::take(
                    &mut snapshot.items,
                ))
                .items;
            }
        }
        self.observations.extend([
            SessionObservation::AgentAttached(false),
            SessionObservation::SetBusy(false),
            SessionObservation::Status {
                phase: HostPhase::NotReady,
                queue_len: 0,
                detail: Some(HostStatusDetail::Failure { failure: status }),
            },
        ]);
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
    pub(super) fn attach_stored(
        &mut self,
        id: plakous_host_protocol::wire::RequestId,
        stored: StoredAttachment<P>,
        text: &str,
    ) -> CliResult<Forwarded<P>> {
        let StoredAttachment {
            session,
            refusal,
            service,
            actions,
            mailbox,
            page,
            deferral,
        } = stored;
        let sessions = Arc::clone(&self.host.sessions);
        let mut connection = WireConnection::on_service(
            service,
            self.out.clone(),
            actions,
            super::super::Leave::Detach,
        )
        .face(self.face)
        .with_frame_output(Some(self.frame_output()))
        .with_snapshot_output(self.snapshot_output.clone())
        .with_runner_labels(Arc::new(move |id| sessions.runner_label(id)))
        .session_baseline_follows(true);
        if let Some(login) = &self.login {
            connection = connection.identified(login.clone());
        }
        if let Some(output) = self.status_output {
            connection = connection
                .with_buffered_output(self.buffered_output)
                .with_attachment_status(output);
        }
        let frame = self.dispatch_frame(text);
        let reading = if let Some(page) = page
            && let Ok(ClientFrame::Request { op, .. }) = frame
        {
            use plakous_host_protocol::wire::{Answer, Outcome};
            match connection.request(id, op) {
                Some(Outcome::Ok(Answer::Attached { attachment })) => {
                    let super::ViewportMode::Page(revision) = page.mode() else {
                        unreachable!("a stored attachment cannot replay a resident viewport");
                    };
                    let start = super::ViewportStart {
                        attachment,
                        revision: super::ViewportRevision {
                            host_generation: self.host.generation,
                            revision,
                            token: uuid::Uuid::new_v4(),
                        },
                        patches: Vec::new(),
                    };
                    self.unlink();
                    connection.history =
                        Some((attachment, page.write(&mut connection.stdout, id, start)?));
                    connection.record_baseline_sent(attachment, None);
                }
                Some(outcome) => self.write(&HostFrame::Response { id, outcome })?,
                None => unreachable!("stored attach is answered synchronously"),
            }
            true
        } else {
            connection.frame_decoded(self.dispatch_frame(text))?
        };
        if connection.holding() {
            self.unlink();
            self.readonly = Some(ReadOnlyLink {
                session,
                refusal,
                connection,
                _mailbox: mailbox,
                _deferral: deferral,
            });
        }
        Ok(Forwarded::Reading(reading))
    }
}
