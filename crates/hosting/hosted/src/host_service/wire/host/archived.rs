//! Archived attachments own only a persisted view, never a runtime or lease.

use super::{
    Arc, CliResult, ClientFrame, DeserializeOwned, Forwarded, HostConnection, HostFrame,
    NodeAddress, Outcome, ProtocolExtension, ReadOnlyLink, RequestRefusal, Serialize, SlotView,
    WireConnection, Write,
};

impl<P: ProtocolExtension, W: Write + Clone> HostConnection<P, W>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    pub(super) fn archived_attachment(
        &mut self,
        node: &NodeAddress,
        text: &str,
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
        let view = match self.host.sessions.archived_view(&session) {
            Ok(Some(view)) => view,
            Ok(None) => return Ok(None),
            Err(refusal) => return Ok(refused(refusal)),
        };
        if view.root != *node {
            return Ok(refused(RequestRefusal::UnknownNode));
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
                connection,
                _mailbox: mailbox,
            });
        }
        Ok(Some(Forwarded::Reading(reading)))
    }
}
