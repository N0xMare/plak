//! Context-status projection and stable request identities.

use crate::input::UserInput;
use crate::{AgentSessionEndpointError, CheckpointId, RuntimeCheckpoint};
use plakous_model::{
    Actor, AttemptId, ConversationItem, ConversationView, Digest, DigestDomain, EntryId,
    HistorySegment, HistorySegmentKind, Provenance, TurnId,
};
pub(super) fn context_status_projection(
    checkpoint: &RuntimeCheckpoint,
    pending_user_input: Option<&UserInput>,
    notice: Option<HistorySegment>,
) -> Result<ConversationView, AgentSessionEndpointError> {
    let canonical = checkpoint.manifest().canonical();
    let Some(input) = pending_user_input else {
        return Ok(canonical.projection());
    };
    let segment = pending_user_segment(checkpoint, input)?;
    let history = if let Some(notice) = notice {
        canonical
            .history()
            .append(notice)
            .map_err(|_| AgentSessionEndpointError::InvalidInput)?
    } else {
        canonical.history().clone()
    };
    let history = history
        .append(segment)
        .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
    Ok(ConversationView::from_shared(
        history,
        canonical.working().clone(),
    ))
}

/// The recent-intent segment a pending user prompt would open on `checkpoint`:
/// its text and images, so the measured request counts each image by the
/// dialect's per-image bound.
pub(super) fn pending_user_segment(
    checkpoint: &RuntimeCheckpoint,
    input: &UserInput,
) -> Result<HistorySegment, AgentSessionEndpointError> {
    input
        .validate("pending user input")
        .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
    let entry_id = EntryId::try_from(context_status_uuid(
        checkpoint.id(),
        b"pending-entry",
        Some(input),
    )?)
    .map_err(|_| AgentSessionEndpointError::Unavailable)?;
    let item = ConversationItem::new(
        entry_id,
        Actor::User,
        input
            .content_blocks()
            .map_err(|_| AgentSessionEndpointError::InvalidInput)?,
        Provenance::UserInput,
    )
    .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
    HistorySegment::new(HistorySegmentKind::RecentIntent, vec![item])
        .map_err(|_| AgentSessionEndpointError::InvalidInput)
}

pub(super) fn context_status_request_ids(
    checkpoint_id: CheckpointId,
    pending_user_input: Option<&UserInput>,
) -> Result<(TurnId, AttemptId), AgentSessionEndpointError> {
    let turn = context_status_uuid(checkpoint_id, b"turn", pending_user_input)?;
    let attempt = context_status_uuid(checkpoint_id, b"attempt", pending_user_input)?;
    Ok((
        TurnId::try_from(turn).map_err(|_| AgentSessionEndpointError::Unavailable)?,
        AttemptId::try_from(attempt).map_err(|_| AgentSessionEndpointError::Unavailable)?,
    ))
}

fn context_status_uuid(
    checkpoint_id: CheckpointId,
    purpose: &[u8],
    pending_user_input: Option<&UserInput>,
) -> Result<uuid::Uuid, AgentSessionEndpointError> {
    let (query, input) = pending_user_input.map_or_else(
        || (b"current".as_slice(), Vec::new()),
        |input| {
            let mut framed = Vec::new();
            input.frame_into(&mut |part| {
                framed.extend_from_slice(
                    &u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes(),
                );
                framed.extend_from_slice(part);
            });
            (b"with-pending-user-input".as_slice(), framed)
        },
    );
    let digest = Digest::sha256(
        DigestDomain::RequestPlan,
        1,
        &[
            b"plakous-agent/context-status-ephemeral-id-v1",
            checkpoint_id.as_uuid().as_bytes(),
            purpose,
            query,
            &input,
        ],
    )
    .map_err(|_| AgentSessionEndpointError::Unavailable)?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(uuid::Uuid::from_bytes(bytes))
}
