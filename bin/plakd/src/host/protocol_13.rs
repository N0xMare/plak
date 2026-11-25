//! Protocol 13 projection at the host's framed boundary.

use std::io::Write;

use plakd_wire_types::v13::{
    Answer, AnswerV13, AttachTranscript, HostFrameV13, LiveRowLength, Observation, ObservationV13,
    OpV13, Outcome, RequestRefusal, RequestRefusalV13, RetainedRevision, TranscriptStream,
};
use plakous_host_protocol::observation::ObservationClass;
use plakous_host_protocol::views::transcript::{
    TranscriptItem, TranscriptScope, TranscriptStreamDelta,
};
use plakous_host_protocol::wire::{self, AttachmentId, RequestId};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::host_service::wire::{
    AttachmentStatus, HistoryBinding, PreparedViewport, TranscriptRead, ViewportMode,
    ViewportRevision, ViewportStart,
};
#[cfg(test)]
use plakous_hosted::host_service::wire::{ViewportRead, WireConnection};
use plakous_hosted::runtime::{LiveRowReadError, RootTranscript};

use super::sessions::history::{self, AttachedHistory, BaselineTarget, HistoryAttachment};
use crate::home::paths::PlakPaths;
#[cfg(test)]
use crate::surface_protocol::PlakExtension;

mod frames;
mod options;
mod viewport;
pub(super) use frames::{decode, encode, observes};
pub(super) use viewport::prepare;

pub(super) fn encode_transcript_scope_expired(id: RequestId) -> Result<String, wire::WireError> {
    wire::encode_line(&HostFrameV13::Response {
        id,
        outcome: Outcome::Refused(Box::new(RequestRefusal::Draft(
            RequestRefusalV13::CursorExpired,
        ))),
    })
}

/// Replay preparation does no paging and holds no reader slot or connection.
#[cfg(test)]
pub(crate) fn prepare_replay(
    paths: &PlakPaths,
    session: &str,
    generation: u64,
    retained: &RetainedRevision,
) -> Option<(HistoryAttachment, ViewportRevision)> {
    if retained.host_generation != generation {
        return None;
    }
    let token = uuid::Uuid::parse_str(&retained.token).ok()?;
    if token.to_string() != retained.token {
        return None;
    }
    let binding = history::replay_binding(paths, session).ok()?;
    Some((
        binding,
        ViewportRevision {
            host_generation: generation,
            revision: retained.revision,
            token,
        },
    ))
}

/// Complete a replay on the writer. A miss goes to first-page preparation.
#[cfg(test)]
pub(crate) fn finish_replay(
    connection: &mut WireConnection<PlakExtension, impl Write>,
    stamp: &ViewportRead,
    id: RequestId,
    (mut binding, retained): (HistoryAttachment, ViewportRevision),
) -> Result<Option<AttachedHistory>, RequestRefusal> {
    let Some(replay) = connection
        .replay_viewport(stamp, binding.fence(), &retained)
        .map_err(wire_refusal)?
    else {
        return Ok(None);
    };
    binding.bind(replay.attachment);
    Ok(Some(AttachedHistory {
        binding,
        frames: replay_frames(id, replay),
    }))
}

fn replay_frames(id: RequestId, replay: ViewportStart) -> Vec<HostFrameV13> {
    let from = retained_revision(&replay.revision);
    let mut to = from.clone();
    if let Some(last) = replay.patches.last() {
        to.revision = last.to_revision;
    }
    let mut frames = vec![history::answer(
        id,
        AnswerV13::Attached {
            attachment: replay.attachment,
            transcript: AttachTranscript::Replay { from, to },
        },
    )];
    frames.extend(patches(replay));
    frames
}

/// Page on a blocking worker. The driver's publication stamp owns no slot
/// or guard, and this work does not borrow the connection's reader or writer.
pub(crate) fn prepare_first_page(
    paths: &PlakPaths,
    session: &str,
    id: RequestId,
    generation: u64,
    root: Option<&RootTranscript>,
) -> Result<AttachedHistory, RequestRefusal> {
    history::attach(
        paths,
        session,
        id,
        BaselineTarget {
            // Allocate space even for the largest assigned id and UUID.
            attachment: AttachmentId::MAX,
            scope: TranscriptScope::Root,
            revision: RetainedRevision {
                host_generation: generation,
                revision: root.map_or(0, |_| u64::MAX),
                token: uuid::Uuid::nil().to_string(),
            },
        },
        root,
    )
}

/// Install a page with any bounded catch-up chain published during its read.
/// A reset or an unbridgeable change requires recapture, not a stale page.
#[cfg(test)]
pub(crate) fn finish_first_page(
    connection: &mut WireConnection<PlakExtension, impl Write>,
    stamp: &ViewportRead,
    id: RequestId,
    page: AttachedHistory,
) -> Result<Option<AttachedHistory>, RequestRefusal> {
    let Some(HostFrameV13::Snapshot(first)) = page.frames.get(1) else {
        return Err(wire_refusal(wire::RequestRefusal::Backpressure));
    };
    let Some(start) = connection
        .install_viewport_page(stamp, page.binding.fence(), first.revision.revision)
        .map_err(wire_refusal)?
    else {
        return Ok(None);
    };
    Ok(Some(first_page_frames(id, page, start)))
}

fn first_page_frames(
    id: RequestId,
    mut page: AttachedHistory,
    start: ViewportStart,
) -> AttachedHistory {
    page.binding.bind(start.attachment);
    page.frames[0] = history::answer(
        id,
        AnswerV13::Attached {
            attachment: start.attachment,
            transcript: AttachTranscript::FirstPage,
        },
    );
    for frame in &mut page.frames[1..] {
        if let HostFrameV13::Snapshot(part) = frame {
            part.attachment = start.attachment;
            part.revision = retained_revision(&start.revision);
        }
    }
    page.frames.extend(patches(start));
    page
}
fn retained_revision(revision: &ViewportRevision) -> RetainedRevision {
    RetainedRevision {
        host_generation: revision.host_generation,
        revision: revision.revision,
        token: revision.token.to_string(),
    }
}

fn patches(start: ViewportStart) -> impl Iterator<Item = HostFrameV13> {
    start
        .patches
        .into_iter()
        .enumerate()
        .map(move |(index, patch)| HostFrameV13::Event {
            attachment: start.attachment,
            seq: index as u64 + 1,
            class: ObservationClass::Addressed,
            observation: Box::new(Observation::from(std::sync::Arc::unwrap_or_clone(patch))),
            client_id: None,
            blobs: Vec::new(),
        })
}

#[cfg(test)]
fn wire_refusal(reason: wire::RequestRefusal) -> RequestRefusal {
    RequestRefusal::try_from(reason).unwrap_or_else(|_| {
        RequestRefusal::session_unavailable("the session could not admit the attachment".into())
    })
}

pub(crate) fn status(
    out: &mut dyn Write,
    attachment: AttachmentId,
    request_id: Option<RequestId>,
    status: AttachmentStatus,
) -> CliResult<()> {
    if let Some(id) = request_id {
        write_frames(
            out,
            [history::answer(id, AnswerV13::AttachmentStatusRequested)],
        )?;
    }
    let frame = HostFrameV13::AttachmentStatus {
        attachment,
        request_id,
        deltas_evicted: status.deltas_evicted,
        patches_skipped: status.patches_skipped,
        baselines_sent: status.baselines_sent,
        seq_gaps: status.seq_gaps,
        bytes_queued: status.bytes_queued,
        queue_len: status.queue_len,
    };
    write_frames(out, [frame])
}

fn write_frames(
    out: &mut dyn Write,
    frames: impl IntoIterator<Item = HostFrameV13>,
) -> CliResult<()> {
    for frame in frames {
        let line = wire::encode_line(&frame).map_err(|error| CliFailure::new(error.to_string()))?;
        out.write_all(line.as_bytes())
            .map_err(|_| CliFailure::new("transcript output failed"))?;
    }
    out.flush()
        .map_err(|_| CliFailure::new("transcript output failed"))
}

pub(crate) fn live_lengths(items: &[TranscriptItem]) -> Vec<LiveRowLength> {
    items
        .iter()
        .filter_map(|item| {
            RootTranscript::live_row_len(item).map(|len| LiveRowLength { row: item.key, len })
        })
        .collect()
}

pub(crate) fn append(delta: TranscriptStreamDelta) -> Option<Observation> {
    let TranscriptStreamDelta::Keyed {
        row,
        offset,
        end,
        text,
    } = delta
    else {
        return None;
    };
    Some(Observation::Draft(ObservationV13::TranscriptStream(
        TranscriptStream {
            row,
            offset,
            end,
            text,
        },
    )))
}

pub(crate) fn read_live_row(
    root: &RootTranscript,
    owner: AttachmentId,
    op: &OpV13,
) -> Option<Outcome> {
    let OpV13::ReadTranscriptRow {
        attachment,
        row,
        offset,
    } = *op
    else {
        return None;
    };
    if attachment != owner {
        return refused(wire::RequestRefusal::NotAttached);
    }
    if !RootTranscript::is_provisional(row) {
        return None; // The history reader owns durable rows.
    }
    match root.read(row, offset) {
        Ok(slice) => Some(Outcome::Ok(Box::new(Answer::Draft(
            AnswerV13::TranscriptRow {
                row,
                offset: slice.offset,
                end: slice.end,
                text: slice.text,
                last: slice.last,
            },
        )))),
        Err(LiveRowReadError::RowGone) => Some(Outcome::Refused(Box::new(RequestRefusal::Draft(
            RequestRefusalV13::CursorExpired,
        )))),
        Err(LiveRowReadError::InvalidOffset) => refused(wire::RequestRefusal::Malformed {
            detail: "invalid transcript row byte offset".into(),
        }),
    }
}

fn refused(reason: wire::RequestRefusal) -> Option<Outcome> {
    RequestRefusal::try_from(reason)
        .ok()
        .map(Box::new)
        .map(Outcome::Refused)
}

#[test]
fn unprojectable_attachment_refusal_is_not_a_cursor_expiry() {
    let refusal = wire_refusal(wire::RequestRefusal::PlacementRefused {
        runner: wire::RunnerLabel::local(),
        reason: Box::new(wire::PlacementReason::RunnerNotPaired),
    });
    assert!(matches!(
        refusal,
        RequestRefusal::Shared(refusal)
            if matches!(refusal.as_inner(), wire::RequestRefusal::SessionUnavailable { .. })
    ));
}
