//! Protocol 13 history requests, deliberately not installed in a listener.
//! Call the handlers off the async reactor (for example, via `spawn_blocking`):
//! SQLite snapshots, rendering, and frame sizing are synchronous.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use plakd_wire_types::v13::{
    Answer, AnswerV13, AttachTranscript, HistoryCursor, HostFrameV13, Outcome, PAGE_TARGET_BYTES,
    RequestRefusal, RequestRefusalV13,
};
use plakous_host_protocol::views::transcript::{RowKey, TranscriptItem};
use plakous_host_protocol::wire::{self, AttachmentId, RequestId};
use plakous_hosted::runtime::RootTranscript;

use crate::home::paths::PlakPaths;
use crate::store::transcript_history::{self, History, HistoryError, Line, Prune, Totals};

mod framing;
pub(crate) use framing::BaselineTarget;
use framing::LiveTail;
mod turns;
use turns::{OlderCut, Turns, turns_at};

struct Page {
    from: u64,
    items: Vec<TranscriptItem>,
    older: Totals,
    cursor: Option<HistoryCursor>,
}

pub(crate) struct HistoryAttachment {
    line: Line,
    attachment: AttachmentId,
}

pub(crate) struct AttachedHistory {
    pub(crate) binding: HistoryAttachment,
    pub(crate) frames: Vec<HostFrameV13>,
}

/// The caller has already resolved an authorized attachment to a product
/// session. A supplied resident root is captured before the store read; the
/// two views may only be merged when their publication watermarks match.
pub(crate) fn attach(
    paths: &PlakPaths,
    session: &str,
    id: RequestId,
    mut target: BaselineTarget,
    root: Option<&RootTranscript>,
) -> Result<AttachedHistory, RequestRefusal> {
    let live = match root {
        None => LiveTail::default(),
        Some(root) => {
            let capture = root
                .capture(session)
                .ok_or_else(|| refusal(HistoryError::StaleCapture))?;
            if target.scope != plakous_host_protocol::views::transcript::TranscriptScope::Root {
                return Err(refusal(HistoryError::StaleCapture));
            }
            target.revision.revision = capture.revision;
            LiveTail::captured(capture)
        }
    };
    let (captured_line, page) = transcript_history::read(paths, session, |history| {
        if live
            .applied
            .is_some_and(|applied| applied != history.applied)
        {
            return Err(HistoryError::StaleCapture);
        }
        let turns = turns_at(&paths.runtime_database(), &history, None)?;
        let page = first(&history, &turns)?;
        Ok((history.line, page))
    })
    .map_err(refusal)?;
    let attachment = target.attachment;
    let parts = framing::baseline(page, target, live).map_err(refusal)?;
    let frames = prepend(
        answer(
            id,
            AnswerV13::Attached {
                attachment,
                transcript: AttachTranscript::FirstPage,
            },
        ),
        parts,
    );
    Ok(AttachedHistory {
        binding: HistoryAttachment {
            line: captured_line,
            attachment,
        },
        frames,
    })
}

pub(crate) fn older(
    paths: &PlakPaths,
    binding: &HistoryAttachment,
    id: RequestId,
    cursor: &HistoryCursor,
) -> Vec<HostFrameV13> {
    let page = transcript_history::read(paths, binding.line.session(), |history| {
        let (line, prune, retained_from, before) = decode_cursor(cursor)?;
        if line != history.line
            || line != binding.line
            || prune != history.prune
            || before > history.applied.get()
        {
            return Err(HistoryError::CursorExpired);
        }
        let turns = turns_at(&paths.runtime_database(), &history, Some(before))?;
        let page = older_page(
            &history,
            &turns,
            retained_from,
            before,
            PAGE_TARGET_BYTES as u64,
        )?;
        Ok(page)
    })
    .and_then(|page| framing::history(page, binding.attachment, id));
    match page {
        Ok(parts) => prepend(answer(id, AnswerV13::HistoryStarted), parts),
        Err(error) => vec![bound_refusal(id, error)],
    }
}

pub(crate) fn read_row(
    paths: &PlakPaths,
    binding: &HistoryAttachment,
    id: RequestId,
    row: RowKey,
    offset: u64,
) -> HostFrameV13 {
    let answer = transcript_history::read(paths, binding.line.session(), |history| {
        if history.line != binding.line {
            return Err(HistoryError::CursorExpired);
        }
        history.row(row)
    })
    .and_then(|mut value| {
        framing::row_slice(
            id,
            row,
            transcript_history::body_mut(&mut value).map_or("", |text| text.as_str()),
            offset,
        )
    });
    answer.unwrap_or_else(|error| bound_refusal(id, error))
}

fn bound_refusal(id: RequestId, error: HistoryError) -> HostFrameV13 {
    let error = if error == HistoryError::UnknownSession {
        HistoryError::CursorExpired
    } else {
        error
    };
    HostFrameV13::Response {
        id,
        outcome: Outcome::Refused(Box::new(refusal(error))),
    }
}

fn refusal(error: HistoryError) -> RequestRefusal {
    let expired = RequestRefusal::Draft(RequestRefusalV13::CursorExpired);
    let reason = match error {
        HistoryError::CursorExpired | HistoryError::UnknownRow => return expired,
        HistoryError::UnknownSession => wire::RequestRefusal::UnknownSession,
        HistoryError::StaleCapture => wire::RequestRefusal::Backpressure,
        HistoryError::InvalidOffset => wire::RequestRefusal::Malformed {
            detail: "invalid transcript row byte offset".into(),
        },
        HistoryError::FrameTooLarge => wire::RequestRefusal::SessionUnavailable {
            detail: "transcript metadata exceeds the frame limit".into(),
        },
        HistoryError::Unavailable => wire::RequestRefusal::SessionUnavailable {
            detail: "transcript history is unavailable".into(),
        },
    };
    RequestRefusal::try_from(reason).unwrap_or(expired)
}

fn prepend(frame: HostFrameV13, parts: Vec<HostFrameV13>) -> Vec<HostFrameV13> {
    std::iter::once(frame).chain(parts).collect()
}

fn first(history: &History<'_>, turns: &Turns) -> Result<Page, HistoryError> {
    project(
        history,
        turns,
        turns.starts.last().copied().unwrap_or(0),
        None,
    )
}

fn older_page(
    history: &History<'_>,
    turns: &Turns,
    retained_from: u64,
    before: u64,
    budget: u64,
) -> Result<Page, HistoryError> {
    if turns.retained_from != retained_from || before == 0 {
        return Err(HistoryError::CursorExpired);
    }
    let end = turns
        .starts
        .binary_search(&before)
        .map_err(|_| HistoryError::CursorExpired)?;
    if let Some(from) = turns.older_from {
        return project(history, turns, from, Some(before));
    }
    let mut cut = OlderCut::new(before, budget);
    for start in turns.starts[..end].iter().rev().copied().chain([0]) {
        if !cut.include(history, start)? {
            break;
        }
    }
    project(history, turns, cut.from, Some(before))
}

fn project(
    history: &History<'_>,
    turns: &Turns,
    from: u64,
    to: Option<u64>,
) -> Result<Page, HistoryError> {
    let older = history.totals(0, Some(from))?;
    let cursor = if older.rows == 0 {
        None
    } else {
        let bytes = serde_json::to_vec(&(&history.line, &history.prune, turns.retained_from, from))
            .map_err(|_| HistoryError::Unavailable)?;
        Some(HistoryCursor(URL_SAFE_NO_PAD.encode(bytes)))
    };
    Ok(Page {
        from,
        items: history.items(from, to)?,
        older,
        cursor,
    })
}

fn decode_cursor(cursor: &HistoryCursor) -> Result<(Line, Prune, u64, u64), HistoryError> {
    if cursor.0.len() > 2048 {
        return Err(HistoryError::CursorExpired);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(&cursor.0)
        .map_err(|_| HistoryError::CursorExpired)?;
    serde_json::from_slice(&bytes).map_err(|_| HistoryError::CursorExpired)
}

fn answer(id: RequestId, answer: AnswerV13) -> HostFrameV13 {
    HostFrameV13::Response {
        id,
        outcome: Outcome::Ok(Box::new(Answer::Draft(answer))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "offline paging fixtures")]
mod tests;
