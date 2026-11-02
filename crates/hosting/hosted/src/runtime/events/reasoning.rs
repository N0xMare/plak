//! Reasoning block lifecycle.

use crate::runtime::turn::flush_assistant_delta;
use crate::runtime::{SealedReasoning, Sender, TurnProjection};
use crate::sessions;
use crate::sessions::SessionRecord;
use crate::surface_protocol::ContentEvent;
use std::time::Instant;

pub(super) fn sealed_reasoning(
    location_key: String,
    text: String,
    started: Instant,
) -> SealedReasoning {
    SealedReasoning {
        location_key,
        text,
        elapsed_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
    }
}

fn observe_thinking_activity(session: &mut SessionRecord, sealed: &SealedReasoning) {
    if sealed.text.trim().is_empty() {
        return;
    }
    let activity = sessions::ThinkingRow {
        location_key: sealed.location_key.clone(),
        text: sealed.text.clone(),
        elapsed_ms: sealed.elapsed_ms,
    };
    session.append_thinking_activity(activity);
}

/// Seals one reasoning block for the surface and holds it until its response
/// settles.
pub(super) fn seal_reasoning_block<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    events: &Sender<P>,
    location: plakous::model::OutputLocation,
) {
    if let Some(sealed) = projection.seal_reasoning(location) {
        let location_key = sealed.location_key.clone();
        projection.hold_thinking(sealed);
        let _ = events.send(ContentEvent::ReasoningSealed { location_key });
    }
}

/// Observes the reasoning a completed response or an ended turn settled: the
/// blocks it sealed, then every admitted location still open, which the stream
/// will now never complete. An unbound location never becomes displayable, so
/// its buffer is dropped rather than flushed.
pub(in crate::runtime) fn settle_live_thinking<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    session: &mut SessionRecord,
    events: &Sender<P>,
) {
    for sealed in projection.settle_thinking() {
        observe_thinking_activity(session, &sealed);
    }
    for sealed in projection.seal_all_reasoning() {
        observe_thinking_activity(session, &sealed);
        let _ = events.send(ContentEvent::ReasoningSealed {
            location_key: sealed.location_key,
        });
    }
}

/// Settles one completed model response. A Content view carries only the
/// response's kind, so this is all the host can do with it there.
///
/// Its reasoning settles before
/// any row of the tools it requested, so a later discarding replay can only
/// drop the response after it.
pub(super) fn settle_model_response<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    session: &mut SessionRecord,
    events: &Sender<P>,
    delta_buf: &mut String,
    last_flush: &mut Instant,
) {
    flush_assistant_delta(delta_buf, last_flush, events, true);
    settle_live_thinking(projection, session, events);
}

/// Drops the reasoning of a response a replay discarded, closing any block a
/// surface still shows open.
pub(super) fn discard_response_reasoning<P: plakous_host_protocol::ProtocolExtension>(
    projection: &mut TurnProjection<P>,
    events: &Sender<P>,
) {
    for location_key in projection.discard_reasoning() {
        let _ = events.send(ContentEvent::ReasoningSealed { location_key });
    }
}

/// Drops the assistant text and reasoning the current request streamed past the
/// last kept response, and tells every surface to do the same.
///
/// A response that already settled has nothing uncommitted. Sending the discard
/// then would wipe that response, so the notice goes out only when this call
/// actually drops text or reasoning.
pub(super) fn discard_uncommitted_response<P: plakous_host_protocol::ProtocolExtension>(
    assistant: &mut String,
    delta_buf: &mut String,
    projection: &mut TurnProjection<P>,
    events: &Sender<P>,
) {
    let had_delta = !delta_buf.is_empty();
    delta_buf.clear();
    let had_tail = assistant.len() > projection.settled_assistant_len;
    assistant.truncate(projection.settled_assistant_len);
    let sealed = projection.discard_reasoning();
    for location_key in sealed.iter().cloned() {
        let _ = events.send(ContentEvent::ReasoningSealed { location_key });
    }
    if had_delta || had_tail || !sealed.is_empty() {
        let _ = events.send(ContentEvent::OutputDiscarded { dim: true });
    }
}

pub(in crate::runtime) fn reasoning_location_key(
    location: plakous::model::OutputLocation,
) -> String {
    format!("{}:{}", location.item_index(), location.block_index())
}

pub(in crate::runtime) const fn reasoning_visibility_admits(
    visibility: plakous::model::ReasoningVisibility,
) -> bool {
    matches!(
        visibility,
        plakous::model::ReasoningVisibility::Visible | plakous::model::ReasoningVisibility::Summary
    )
}

#[cfg(test)]
mod tests;
