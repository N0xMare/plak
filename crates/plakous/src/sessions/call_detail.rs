//! Bounded, redacted read of one tool call's input and result by call id.

use super::{CallId, ToolName, fmt};
use plakous_model::{
    CanonicalHistory, ContentBlock, ConversationItem, HistorySegment, ToolResultPart,
};
use plakous_tools::internal::preview as preview_redaction;
use std::borrow::Cow;

/// Most bytes of input or of output one call-detail read returns per side.
pub const MAX_CALL_DETAIL_SIDE_BYTES: usize = 256 * 1024;

/// Extra bytes redacted past the side bound before clipping, so a credential
/// shorter than this margin is redacted whole rather than cut into an
/// unrecognizable prefix. When the source is longer than the window, this many
/// bytes of the redacted text are held back from its end.
const REDACTION_MARGIN_BYTES: usize = 16 * 1024;

/// The most source bytes one side reads and redacts. Raw argument text and
/// result text are cut to this window before the text redactor runs. Parsed
/// JSON is redacted as a value and pretty-printed by a writer that stops
/// after this many bytes, so the unread tail is not copied or visited.
const REDACTION_WINDOW_BYTES: usize = MAX_CALL_DETAIL_SIDE_BYTES + REDACTION_MARGIN_BYTES;

/// One side (input or output) of a call detail: redacted text, clipped to at
/// most [`MAX_CALL_DETAIL_SIDE_BYTES`] at a character boundary.
#[derive(Clone, Eq, PartialEq)]
pub struct SessionCallDetailText {
    text: String,
    truncated: bool,
}

impl SessionCallDetailText {
    /// Returns the redacted, bounded text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the text is incomplete: the redacted text ran past the bound,
    /// or the source ran past the redaction window (then the window's last
    /// 16 KiB after redaction are held back too).
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    fn bounded(source: &str) -> Self {
        let window = prefix_at_char_boundary(source, REDACTION_WINDOW_BYTES);
        Self::from_window(window, window.len() < source.len())
    }

    /// Redacts `window`, then clips. `clipped` says the source continued past
    /// the window. The redactor never saw that continuation, so the tail of its
    /// output may hold the unrecognized head of a credential the window cut;
    /// redaction also shrinks text, which pulls that tail toward the bound.
    /// A clipped window therefore keeps back the last
    /// [`REDACTION_MARGIN_BYTES`] of the redacted text, whatever it shrank to.
    fn from_window(window: &str, clipped: bool) -> Self {
        Self::clip_redacted(preview_redaction::text(window), clipped)
    }

    /// Clips text that is already redacted. Parsed JSON uses this: each string
    /// leaf was redacted before it was written, and the writer stopped at the
    /// window, so a second text pass would only re-scan the prefix.
    fn clip_redacted(redacted: String, clipped: bool) -> Self {
        let limit = if clipped {
            MAX_CALL_DETAIL_SIDE_BYTES.min(redacted.len().saturating_sub(REDACTION_MARGIN_BYTES))
        } else {
            MAX_CALL_DETAIL_SIDE_BYTES
        };
        let end = char_boundary_end(&redacted, limit);
        let truncated = clipped || end < redacted.len();
        let mut text = redacted;
        text.truncate(end);
        Self { text, truncated }
    }
}

impl fmt::Debug for SessionCallDetailText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionCallDetailText")
            .field("text", &"<redacted>")
            .field("bytes", &self.text.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// One tool call's input and result as committed canonical history holds
/// them, redacted with the shared preview redactor and bounded per side.
#[derive(Clone, Eq, PartialEq)]
pub struct SessionCallDetail {
    call_id: CallId,
    canonical_name: ToolName,
    exposed_name: ToolName,
    input: SessionCallDetailText,
    output: SessionCallDetailText,
}

impl SessionCallDetail {
    /// Returns the canonical call identity.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the canonical executable name of the call.
    #[must_use]
    pub const fn canonical_name(&self) -> &ToolName {
        &self.canonical_name
    }

    /// Returns the exact name the model called; the call is labelled by it.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }

    /// Returns the call's arguments. Parsed JSON is pretty-printed only up to
    /// the redaction window; otherwise this is the raw argument text.
    #[must_use]
    pub const fn input(&self) -> &SessionCallDetailText {
        &self.input
    }

    /// Returns the terminal result text the model saw. Non-text parts appear
    /// as a bracketed media-type marker.
    #[must_use]
    pub const fn output(&self) -> &SessionCallDetailText {
        &self.output
    }
}

impl fmt::Debug for SessionCallDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionCallDetail")
            .field("call_id", &self.call_id)
            .field("canonical_name", &self.canonical_name)
            .field("exposed_name", &self.exposed_name)
            .field("input", &self.input)
            .field("output", &self.output)
            .finish()
    }
}

/// The answer to one call-detail read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCallDetailOutcome {
    /// The call and its result are in the session's committed canonical
    /// history (a committed exchange always holds both).
    Found(SessionCallDetail),
    /// No call with this id is in the session's committed canonical history:
    /// it was compacted, pruned or not carried, is not committed yet, or
    /// belongs to another session. The read never consults another session.
    NotInCanonicalHistory,
}

pub(super) fn project_call_detail(
    history: &CanonicalHistory,
    call_id: CallId,
) -> SessionCallDetailOutcome {
    let mut call = None;
    let mut output = None;
    let items = history.segments().iter().flat_map(HistorySegment::items);
    for block in items.flat_map(ConversationItem::blocks) {
        match block {
            ContentBlock::ToolCall(block) if call.is_none() && block.call_id() == call_id => {
                call = Some(block);
            }
            ContentBlock::ToolResult(block) if output.is_none() && block.call_id() == call_id => {
                output = Some(block);
            }
            _ => {}
        }
    }
    let (Some(call), Some(output)) = (call, output) else {
        return SessionCallDetailOutcome::NotInCanonicalHistory;
    };
    let input = match call.arguments() {
        Some(arguments) => {
            let (pretty, clipped) =
                preview_redaction::pretty_bounded(arguments, REDACTION_WINDOW_BYTES);
            SessionCallDetailText::clip_redacted(pretty, clipped)
        }
        None => SessionCallDetailText::bounded(call.raw_arguments()),
    };
    SessionCallDetailOutcome::Found(SessionCallDetail {
        call_id,
        canonical_name: call.canonical_name().clone(),
        exposed_name: call.exposed_name().clone(),
        input,
        output: {
            let (window, clipped) = result_text(output.parts());
            SessionCallDetailText::from_window(&window, clipped)
        },
    })
}

/// The result text the model saw, read only up to the redaction window:
/// returns the text and whether more content (or more parts) followed.
fn result_text(parts: &[ToolResultPart]) -> (String, bool) {
    let mut text = String::new();
    for part in parts {
        if !text.is_empty() && !push_within_window(&mut text, "\n") {
            return (text, true);
        }
        let marker = |kind: &str, media_type: &str| format!("[{kind} {media_type}]");
        let piece = match part {
            ToolResultPart::Text(block) => Cow::Borrowed(block.text()),
            ToolResultPart::Image(block) => Cow::Owned(marker("image", block.media_type())),
            ToolResultPart::Document(block) => Cow::Owned(marker("document", block.media_type())),
            ToolResultPart::Audio(block) => Cow::Owned(marker("audio", block.media_type())),
            ToolResultPart::Opaque(_) => Cow::Borrowed("[opaque content]"),
            _ => Cow::Borrowed("[unsupported content]"),
        };
        if !push_within_window(&mut text, &piece) {
            return (text, true);
        }
    }
    (text, false)
}

/// Appends as much of `piece` as fits in the redaction window; false when
/// some of it did not fit.
fn push_within_window(text: &mut String, piece: &str) -> bool {
    let kept = prefix_at_char_boundary(piece, REDACTION_WINDOW_BYTES.saturating_sub(text.len()));
    text.push_str(kept);
    kept.len() == piece.len()
}

fn prefix_at_char_boundary(value: &str, limit: usize) -> &str {
    &value[..char_boundary_end(value, limit)]
}

fn char_boundary_end(value: &str, limit: usize) -> usize {
    if value.len() <= limit {
        return value.len();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests;
