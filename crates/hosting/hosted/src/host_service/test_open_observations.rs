//! Preserve the selected fixture's real opening observations until its first attachment.

use super::{HostService, ProtocolExtension, SessionObservation, fanout::Publication};
use std::sync::OnceLock;

static SESSION: OnceLock<String> = OnceLock::new();

/// Hold the selected session's opening recovery notice until its first attachment.
/// Opening state and the transcript must publish normally to admit a viewport.
///
/// # Errors
/// The selection is not a UUID, or this process already selected a fixture.
pub fn install(session: String) -> Result<(), &'static str> {
    uuid::Uuid::parse_str(&session).map_err(|_| "recovery fixture session must be a UUID")?;
    SESSION
        .set(session)
        .map_err(|_| "recovery fixture already selected")
}

pub(super) struct OpeningRecovery<P: ProtocolExtension> {
    service: HostService<P>,
    held: Vec<Publication<P>>,
}

impl<P: ProtocolExtension> OpeningRecovery<P> {
    pub(super) fn selected(session: &str, service: &HostService<P>) -> Option<Self> {
        (SESSION.get().is_some_and(|selected| selected == session)).then(|| Self {
            service: service.clone(),
            held: Vec::new(),
        })
    }

    pub(super) fn interleave(&mut self, batch: &mut Vec<Publication<P>>) -> bool {
        if self
            .service
            .lock()
            .attachments
            .values()
            .any(|held| !held.dropped)
        {
            batch.splice(..0, self.held.drain(..));
            return true;
        }
        let mut keep = Vec::with_capacity(batch.len());
        for event in batch.drain(..) {
            let recovery = matches!(event.observation, SessionObservation::Recovered { .. })
                || matches!(&event.observation, SessionObservation::Row(row)
                    if row.text().is_some_and(|text| text.contains("recovered an interrupted turn")));
            if recovery {
                self.held.push(event);
            } else {
                keep.push(event);
            }
        }
        *batch = keep;
        false
    }
}
