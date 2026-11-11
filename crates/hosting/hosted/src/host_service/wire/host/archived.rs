//! Stored attachments own only a persisted view, never a runtime or lease.

use plakous_host_protocol::{
    SessionObservation,
    status::{HostPhase, HostStatusDetail},
    views::transcript::{NoticeScope, TranscriptItem, TranscriptRow},
};

use super::{
    Arc, CliResult, ClientFrame, DeserializeOwned, Forwarded, HostConnection, HostFrame,
    NodeAddress, Outcome, ProtocolExtension, ReadOnlyLink, RequestRefusal, Serialize, SlotView,
    StoredSessionView, WireConnection, Write,
};

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
    pub(super) fn stored_attachment(
        &mut self,
        node: &NodeAddress,
        text: &str,
        failure: Option<&crate::error::CliFailure>,
    ) -> CliResult<Option<Forwarded<P>>> {
        let refused = |refusal| Some(Forwarded::Answered(Outcome::Refused(refusal)));
        let resident = self.host.lock().iter().find_map(|slot| match slot.peek() {
            SlotView::Resident(session) if session.root().tree_id == node.tree_id => {
                Some(session.session_id())
            }
            _ => None,
        });
        let session = match resident.map_or_else(
            || self.host.sessions.session_of(node),
            |session| Ok(Some(session)),
        ) {
            Ok(Some(session)) => session,
            Ok(None) => return Ok(None),
            Err(detail) => return Ok(refused(RequestRefusal::SessionUnavailable { detail })),
        };
        let mut view = match self.host.sessions.stored_view(&session) {
            Ok(Some(view)) => view,
            Ok(None) => return Ok(None),
            Err(refusal) => return Ok(refused(refusal)),
        };
        let cached = self
            .host
            .lock()
            .iter()
            .find(|slot| super::serves(slot, &session))
            .and_then(|slot| slot.open_failure());
        let failure = failure.or(cached.as_ref());
        // Ownership refusals must be rechecked when their holder goes away,
        // even when the product can no longer resolve the old holder's PID.
        if !view.archived
            && failure.is_some_and(|failure| failure.code() == Some("tree_hosted_elsewhere"))
        {
            return Ok(None);
        }
        let refusal = if view.archived {
            RequestRefusal::SessionArchived {
                session: session.clone(),
            }
        } else if let Some(failure) = failure {
            self.host.sessions.refusal(&session, failure)
        } else {
            return Ok(None);
        };
        if !matches!(
            refusal,
            RequestRefusal::SessionArchived { .. }
                | RequestRefusal::SessionUnavailable { .. }
                | RequestRefusal::PlacementRefused { .. }
                | RequestRefusal::BindingRefused { .. }
        ) {
            return Ok(None);
        }
        if view.root != *node {
            return Ok(refused(RequestRefusal::UnknownNode));
        }
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
        let sessions = Arc::clone(&self.host.sessions);
        let mut connection = WireConnection::on_service(
            service,
            self.out.clone(),
            actions,
            super::super::Leave::Detach,
        )
        .face(self.face)
        .with_runner_labels(Arc::new(move |id| sessions.runner_label(id)))
        .session_baseline_follows(true);
        if let Some(login) = &self.login {
            connection = connection.identified(login.clone());
        }
        let reading = connection.frame(Ok(text.to_owned()))?;
        if connection.holding() {
            self.unlink();
            self.readonly = Some(ReadOnlyLink {
                session: view.session,
                refusal,
                connection,
                _mailbox: mailbox,
            });
        }
        Ok(Some(Forwarded::Reading(reading)))
    }
}
