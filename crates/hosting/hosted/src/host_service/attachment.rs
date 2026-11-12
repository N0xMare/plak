//! Attachment correlation tags and attach/replay classification.

use super::{
    AttachmentId, Face, LOCAL_ATTACHMENT, ObservationClass, ProtocolExtension, REMOTE_ATTACHMENT,
    SessionObservation, TranscriptScope,
};

/// The origin tag an attachment's intents carry, so the session's
/// `IntentStarted` names the attachment its answers belong to.
pub(super) fn origin(attachment: AttachmentId, face: Face) -> String {
    let prefix = if face == Face::Remote {
        REMOTE_ATTACHMENT
    } else {
        LOCAL_ATTACHMENT
    };
    format!("{prefix}{attachment}")
}

/// The attachment a correlation names, and the client's own correlation
/// inside it.
pub(super) fn attachment_of(correlation: &str) -> Option<(AttachmentId, Option<String>)> {
    let rest = correlation
        .strip_prefix(REMOTE_ATTACHMENT)
        .or_else(|| correlation.strip_prefix(LOCAL_ATTACHMENT))?;
    let (id, client) = match rest.split_once(':') {
        Some((id, client)) => (id, Some(client.to_owned())),
        None => (rest, None),
    };
    Some((id.parse().ok()?, client))
}

/// Whether an attach re-emits `observation` from what the service keeps
/// (states, the root baseline, approvals, the credential wait), so a
/// replay must not send it again.
pub(super) fn kept_for_attach<P: ProtocolExtension>(observation: &SessionObservation<P>) -> bool {
    observation.class() == ObservationClass::State
        || matches!(
            observation,
            SessionObservation::ApprovalRequired { .. }
                | SessionObservation::ApprovalResolved { .. }
                | SessionObservation::CredentialWait { .. }
                | SessionObservation::ClearView
                | SessionObservation::SessionOpening
                | SessionObservation::SessionOpened { .. }
        )
        || matches!(
            observation,
            SessionObservation::PresentationSnapshot(snapshot)
                if snapshot.scope == TranscriptScope::Root
        )
        || matches!(
            observation,
            SessionObservation::TranscriptPatch(patch) if patch.scope == TranscriptScope::Root
        )
}
