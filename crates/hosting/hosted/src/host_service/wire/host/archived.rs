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
}

impl<P: ProtocolExtension> StoredAttachment<P> {
    pub(super) fn new(
        mut view: StoredSessionView<P>,
        refusal: RequestRefusal,
        failure: Option<&crate::error::CliFailure>,
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
            service.observe(observation);
        }
        Self {
            session: view.session,
            refusal,
            service,
            actions,
            mailbox,
        }
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
        stored: StoredAttachment<P>,
        text: &str,
    ) -> CliResult<Forwarded<P>> {
        let StoredAttachment {
            session,
            refusal,
            service,
            actions,
            mailbox,
        } = stored;
        let sessions = Arc::clone(&self.host.sessions);
        let mut connection = WireConnection::on_service(
            service,
            self.out.clone(),
            actions,
            super::super::Leave::Detach,
        )
        .face(self.face)
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
        let reading = connection.frame(Ok(text.to_owned()))?;
        if connection.holding() {
            self.unlink();
            self.readonly = Some(ReadOnlyLink {
                session,
                refusal,
                connection,
                _mailbox: mailbox,
            });
        }
        Ok(Forwarded::Reading(reading))
    }
}
