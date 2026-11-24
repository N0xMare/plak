//! Draft transcript DTO projection. No listener selects this path.

use plakd_wire_types::v13::{
    Answer, AnswerV13, LiveRowLength, Observation, ObservationV13, OpV13, Outcome, RequestRefusal,
    RequestRefusalV13, TranscriptStream,
};
use plakous_host_protocol::views::transcript::{TranscriptItem, TranscriptStreamDelta};
use plakous_host_protocol::wire::{self, AttachmentId};
use plakous_hosted::runtime::{LiveRowReadError, RootTranscript};

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
