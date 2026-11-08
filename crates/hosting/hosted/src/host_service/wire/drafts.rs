//! Host-wide wire identity reservation, independent of attachment delivery.

use std::io::Write;

use plakous_host_protocol::wire::{Answer, AttachmentId, ClientDraftId, Outcome, RequestRefusal};
use plakous_host_protocol::{ProtocolExtension, SessionIntent};

use super::super::drafts::{DraftReservation, DraftReservationError, MAX_DRAFT_IDS};
use super::WireConnection;

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Submission<'a> {
    Prompt {
        draft: &'a plakous_host_protocol::Draft,
    },
    FollowUp {
        draft: &'a plakous_host_protocol::Draft,
    },
    Steer {
        draft: &'a plakous_host_protocol::Draft,
    },
    MailTopologyNode {
        agent_id: &'a str,
        message: &'a str,
        urgent: bool,
    },
}

fn submission<P: ProtocolExtension>(intent: &SessionIntent<P>) -> Option<Submission<'_>> {
    Some(match intent {
        SessionIntent::Prompt(draft) => Submission::Prompt { draft },
        SessionIntent::FollowUp(draft) => Submission::FollowUp { draft },
        SessionIntent::Steer(draft) => Submission::Steer { draft },
        SessionIntent::MailTopologyNode {
            agent_id,
            message,
            urgent,
        } => Submission::MailTopologyNode {
            agent_id,
            message,
            urgent: *urgent,
        },
        _ => return None,
    })
}

impl<P: ProtocolExtension, W: Write> WireConnection<P, W> {
    pub(super) fn reserve_draft(
        &self,
        attachment: AttachmentId,
        intent: &SessionIntent<P>,
        client_id: Option<ClientDraftId>,
    ) -> Result<Option<ClientDraftId>, Outcome<P>> {
        let Some(registry) = &self.drafts else {
            return Ok(None);
        };
        let refused = |detail: &str| {
            Outcome::Refused(RequestRefusal::Malformed {
                detail: detail.to_owned(),
            })
        };
        let Some(submission) = submission(intent) else {
            return if client_id.is_none() {
                Ok(None)
            } else {
                Err(refused(
                    "client_id is only valid for draft and topology mail submissions",
                ))
            };
        };
        let client_id = client_id
            .ok_or_else(|| refused("draft and topology mail submissions require client_id"))?;
        let input = serde_json::to_vec(&(
            self.session.as_deref(),
            self.service.wire_target(attachment),
            submission,
        ))
        .map_err(|_| refused("draft input could not be encoded"))?;
        let digest = plakous::model::sha256_bytes(&[&input]);
        match registry.reserve(client_id, digest) {
            Ok(DraftReservation::Duplicate) => {
                return Err(Outcome::Ok(Answer::Accepted { duplicate: true }));
            }
            Ok(DraftReservation::Pending(pending)) => {
                self.service.begin_wire_draft(client_id, pending);
            }
            Err(DraftReservationError::Pending) => {
                return Err(Outcome::Refused(RequestRefusal::DraftPending { client_id }));
            }
            Err(DraftReservationError::Conflict) => {
                return Err(refused(
                    "client_id was used with different session, target or content",
                ));
            }
            Err(DraftReservationError::Full) => {
                return Err(Outcome::Refused(RequestRefusal::DraftRegistryFull {
                    limit: MAX_DRAFT_IDS,
                }));
            }
        }
        Ok(Some(client_id))
    }
}
