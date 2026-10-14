//! Context-status projection and stable request identities.

use crate::runtime::control::validate_command_text;
use crate::{AgentSessionEndpointError, CheckpointId, RuntimeCheckpoint};
use plakous_model::{
    Actor, AttemptId, ContentBlock, ConversationItem, ConversationView, Digest, DigestDomain,
    EntryId, HistorySegment, HistorySegmentKind, Provenance, TextBlock, TurnId,
};
pub(super) fn context_status_projection(
    checkpoint: &RuntimeCheckpoint,
    pending_user_input: Option<&str>,
    notice: Option<HistorySegment>,
) -> Result<ConversationView, AgentSessionEndpointError> {
    let canonical = checkpoint.manifest().canonical();
    let Some(text) = pending_user_input else {
        return Ok(canonical.projection());
    };
    let segment = pending_user_segment(checkpoint, text)?;
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

/// The recent-intent segment a pending user prompt would open on `checkpoint`.
pub(super) fn pending_user_segment(
    checkpoint: &RuntimeCheckpoint,
    text: &str,
) -> Result<HistorySegment, AgentSessionEndpointError> {
    validate_command_text(text, "pending user input")
        .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
    let entry_id = EntryId::try_from(context_status_uuid(
        checkpoint.id(),
        b"pending-entry",
        Some(text),
    )?)
    .map_err(|_| AgentSessionEndpointError::Unavailable)?;
    let item = ConversationItem::new(
        entry_id,
        Actor::User,
        vec![ContentBlock::Text(
            TextBlock::new(text.to_owned(), None)
                .map_err(|_| AgentSessionEndpointError::InvalidInput)?,
        )],
        Provenance::UserInput,
    )
    .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
    HistorySegment::new(HistorySegmentKind::RecentIntent, vec![item])
        .map_err(|_| AgentSessionEndpointError::InvalidInput)
}

pub(super) fn context_status_request_ids(
    checkpoint_id: CheckpointId,
    pending_user_input: Option<&str>,
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
    pending_user_input: Option<&str>,
) -> Result<uuid::Uuid, AgentSessionEndpointError> {
    let (query, text) = pending_user_input
        .map_or((b"current".as_slice(), b"".as_slice()), |text| {
            (b"with-pending-user-input".as_slice(), text.as_bytes())
        });
    let digest = Digest::sha256(
        DigestDomain::RequestPlan,
        1,
        &[
            b"plakous-agent/context-status-ephemeral-id-v1",
            checkpoint_id.as_uuid().as_bytes(),
            purpose,
            query,
            text,
        ],
    )
    .map_err(|_| AgentSessionEndpointError::Unavailable)?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(uuid::Uuid::from_bytes(bytes))
}
