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
mod tests {
    use super::{
        MAX_CALL_DETAIL_SIDE_BYTES, REDACTION_WINDOW_BYTES, SessionCallDetailOutcome,
        SessionCallDetailText, project_call_detail, result_text,
    };
    use plakous_model::{
        Actor, CallId, CanonicalHistory, ContentBlock, ConversationItem, EntryId, HistorySegment,
        HistorySegmentKind, Provenance, TextBlock, ToolCallBlock, ToolCallState, ToolCorrelation,
        ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind, ToolResultPart,
    };
    use uuid::Uuid;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn call_id(value: u128) -> TestResult<CallId> {
        Ok(CallId::try_from(Uuid::from_u128(value))?)
    }

    fn exchange(call: u128, arguments: &str, result: &[&str]) -> TestResult<HistorySegment> {
        let id = call_id(call)?;
        let items = vec![
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(call << 8))?,
                Actor::Assistant,
                vec![ContentBlock::ToolCall(ToolCallBlock::new(
                    id,
                    ToolName::new("exec_command")?,
                    ToolExecutionOwner::PlakLocal,
                    ToolCallState::Requested,
                    ToolCorrelation::default(),
                    arguments,
                )?)],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128((call << 8) | 1))?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock::new(
                    id,
                    ToolExecutionOwner::PlakLocal,
                    ToolResultKind::Success,
                    result
                        .iter()
                        .map(|part| Ok(ToolResultPart::Text(TextBlock::new(*part, None)?)))
                        .collect::<TestResult<Vec<_>>>()?,
                    ToolCorrelation::default(),
                )?)],
                Provenance::ModelOutput,
            )?,
        ];
        Ok(HistorySegment::new(
            HistorySegmentKind::ToolExchange,
            items,
        )?)
    }

    fn found(outcome: SessionCallDetailOutcome) -> TestResult<super::SessionCallDetail> {
        match outcome {
            SessionCallDetailOutcome::Found(detail) => Ok(detail),
            SessionCallDetailOutcome::NotInCanonicalHistory => Err("call not found".into()),
        }
    }

    #[test]
    fn returns_full_input_and_output_of_the_addressed_call() -> TestResult {
        let history = CanonicalHistory::new(vec![
            exchange(1, r#"{"cmd":"ls"}"#, &["first output"])?,
            exchange(2, r#"{"cmd":"cargo test"}"#, &["line 1\nline 2"])?,
        ])?;
        let detail = found(project_call_detail(&history, call_id(2)?))?;
        assert_eq!(detail.call_id(), call_id(2)?);
        assert_eq!(detail.exposed_name().as_str(), "exec_command");
        assert_eq!(detail.input().text(), "{\n  \"cmd\": \"cargo test\"\n}");
        assert!(!detail.input().truncated());
        let output = detail.output();
        assert_eq!(output.text(), "line 1\nline 2");
        assert!(!output.truncated());
        Ok(())
    }

    #[test]
    fn an_unknown_call_id_is_not_in_canonical_history() -> TestResult {
        let history = CanonicalHistory::new(vec![exchange(4, r#"{"cmd":"ls"}"#, &["ok"])?])?;
        assert_eq!(
            project_call_detail(&history, call_id(5)?),
            SessionCallDetailOutcome::NotInCanonicalHistory
        );
        assert_eq!(
            project_call_detail(&CanonicalHistory::new(Vec::new())?, call_id(4)?),
            SessionCallDetailOutcome::NotInCanonicalHistory
        );
        Ok(())
    }

    #[test]
    fn both_sides_are_redacted_like_session_log_previews() -> TestResult {
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
        let history = CanonicalHistory::new(vec![exchange(
            6,
            &format!(
                r#"{{"cmd":"curl -H 'Authorization: Bearer {secret}'","api_key":"{secret}"}}"#
            ),
            &[&format!("token={secret}\nexported OPENAI_API_KEY={secret}")],
        )?])?;
        let detail = found(project_call_detail(&history, call_id(6)?))?;
        let output = detail.output();
        for text in [detail.input().text(), output.text()] {
            assert!(!text.contains(secret), "{text}");
            assert!(text.contains("[redacted]"), "{text}");
        }
        assert!(
            detail
                .input()
                .text()
                .contains("\"api_key\": \"[redacted]\"")
        );
        assert!(!format!("{detail:?}").contains("curl"));
        Ok(())
    }

    #[test]
    fn each_side_is_bounded_with_an_explicit_truncation_flag() -> TestResult {
        // Canonical JSON bounds one string at 256 KiB, so a committed result
        // exceeds the side bound only across parts, and committed raw
        // arguments hardly at all; the input bound is exercised directly.
        let half = "é".repeat(70 * 1024);
        let history =
            CanonicalHistory::new(vec![exchange(7, r#"{"cmd":"cat big"}"#, &[&half, &half])?])?;
        let detail = found(project_call_detail(&history, call_id(7)?))?;
        assert!(!detail.input().truncated());
        let long_input = SessionCallDetailText::bounded(&format!("{half}{half}"));
        for side in [detail.output(), &long_input] {
            assert!(side.truncated());
            assert!(side.text().len() <= MAX_CALL_DETAIL_SIDE_BYTES);
            assert!(side.text().len() > MAX_CALL_DETAIL_SIDE_BYTES - 2);
        }
        let exact = SessionCallDetailText::bounded(&"a".repeat(MAX_CALL_DETAIL_SIDE_BYTES));
        assert!(!exact.truncated());
        Ok(())
    }

    #[test]
    fn a_secret_straddling_the_bound_is_redacted_before_clipping() {
        // Clipping first would keep `sk-` plus 17 characters, too short for the
        // key rule to recognize, and leak them.
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
        let source = format!("{} {secret}", "x".repeat(MAX_CALL_DETAIL_SIDE_BYTES - 21));
        let side = SessionCallDetailText::bounded(&source);
        assert!(!side.text().contains("sk-abc"), "secret prefix leaked");
        assert!(side.text().ends_with("[redacted]"));
        assert!(!side.truncated());
    }

    /// A prefix that redaction shrinks by more than the margin, then a private
    /// key that starts inside the window and is cut by its end.
    fn shrinking_prefix_then_cut_key(prefix_target: usize) -> (String, String) {
        let token = format!("sk-{}", "a".repeat(200));
        let mut prefix = String::new();
        while prefix.len() < prefix_target {
            prefix.push_str(&token);
            prefix.push(' ');
        }
        let key = format!(
            "-----BEGIN {armor}-----\n{}\n-----END {armor}-----",
            "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC".repeat(100),
            armor = concat!("PRIVATE", " KEY"),
        );
        (prefix, key)
    }

    #[test]
    fn shrinking_redaction_does_not_expose_a_key_cut_by_the_window() {
        let (mut source, key) = shrinking_prefix_then_cut_key(100 * 1024);
        let key_start = REDACTION_WINDOW_BYTES - 1500;
        source.push_str(&"x".repeat(key_start - source.len()));
        source.push_str(&key);
        let side = SessionCallDetailText::bounded(&source);
        assert!(side.truncated());
        assert!(
            !side.text().contains("MIIEvQIBADANBgkq"),
            "private key body leaked: {} bytes returned",
            side.text().len()
        );
        assert!(!side.text().contains("sk-aaaa"));
        // Only the unscanned margin is held back, not the whole window.
        assert!(side.text().len() > 100 * 1024);
    }

    #[test]
    fn shrinking_redaction_across_result_parts_does_not_expose_a_cut_key() -> TestResult {
        let (part1, key) = shrinking_prefix_then_cut_key(200 * 1024);
        let key_start = REDACTION_WINDOW_BYTES - 1500;
        let part2 = format!("{}{key}", "x".repeat(key_start - part1.len() - 1));
        let history = CanonicalHistory::new(vec![exchange(
            9,
            r#"{"cmd":"cat keys"}"#,
            &[&part1, &part2],
        )?])?;
        let detail = found(project_call_detail(&history, call_id(9)?))?;
        assert!(detail.output().truncated());
        assert!(
            !detail.output().text().contains("MIIEvQIBADANBgkq"),
            "private key body leaked through canonical read"
        );
        Ok(())
    }

    #[test]
    fn result_parts_past_the_window_are_not_read_and_count_as_truncation() -> TestResult {
        let part = "p".repeat(200 * 1024);
        let parts = [part.as_str(), part.as_str(), part.as_str()]
            .iter()
            .map(|part| Ok(ToolResultPart::Text(TextBlock::new(*part, None)?)))
            .collect::<TestResult<Vec<_>>>()?;
        let (window, clipped) = result_text(&parts);
        assert!(clipped);
        assert_eq!(window.len(), REDACTION_WINDOW_BYTES);

        // A window filled exactly with more parts after it is still clipped.
        let exact = "q".repeat(REDACTION_WINDOW_BYTES);
        let parts = [exact.as_str(), "tail"]
            .iter()
            .map(|part| Ok(ToolResultPart::Text(TextBlock::new(*part, None)?)))
            .collect::<TestResult<Vec<_>>>()?;
        assert_eq!(result_text(&parts), (exact.clone(), true));
        let (text, clipped) = result_text(&parts[..1]);
        assert!(!clipped);
        assert_eq!(text.len(), REDACTION_WINDOW_BYTES);
        Ok(())
    }

    #[test]
    fn a_source_that_fits_the_window_is_clipped_at_the_bound_only() {
        let source = "a".repeat(REDACTION_WINDOW_BYTES);
        let side = SessionCallDetailText::bounded(&source);
        assert!(side.truncated());
        assert_eq!(side.text().len(), MAX_CALL_DETAIL_SIDE_BYTES);
        let longer = "a".repeat(REDACTION_WINDOW_BYTES + 1);
        let side = SessionCallDetailText::bounded(&longer);
        assert!(side.truncated());
        assert_eq!(side.text().len(), MAX_CALL_DETAIL_SIDE_BYTES);
    }

    // REVIEW-CD2 probes, kept as regressions.

    fn probe_credentials() -> Vec<(&'static str, String, &'static str)> {
        vec![
            ("sk", format!("sk-{}", "Q7".repeat(30)), "Q7Q7"),
            (
                "bearer",
                format!("Authorization: Bearer {}", "Z9".repeat(40)),
                "Z9Z9",
            ),
            ("password", format!("password={}", "P4".repeat(20)), "P4P4"),
            (
                "url",
                format!("https://user:{}@host", "W3".repeat(20)),
                "W3W3",
            ),
            (
                "pem",
                format!(
                    "-----BEGIN {armor}-----\n{}\n-----END {armor}-----",
                    "K6".repeat(1600),
                    armor = concat!("PRIVATE", " KEY"),
                ),
                "K6K6",
            ),
            (
                "jwt",
                format!(
                    "eyJhbGciOiJIUzI1NiJ9.eyJ{}.{}",
                    "J8".repeat(2000),
                    "S5".repeat(30)
                ),
                "S5S5",
            ),
        ]
    }

    #[test]
    fn probe_offsets_around_window_end_never_expose_a_short_credential() {
        let token = format!("sk-{}", "a".repeat(200));
        let mut prefix = String::new();
        while prefix.len() < 100 * 1024 {
            prefix.push_str(&token);
            prefix.push(' ');
        }
        let mut offsets: Vec<usize> = (0..8)
            .map(|i| REDACTION_WINDOW_BYTES - 20_000 + i * 2_503)
            .collect();
        offsets.extend((0..15).map(|i| REDACTION_WINDOW_BYTES - 40 + i * 5));
        offsets.extend([
            REDACTION_WINDOW_BYTES - 3300,
            REDACTION_WINDOW_BYTES - 16 * 1024 - 1,
            REDACTION_WINDOW_BYTES - 16 * 1024 + 1,
        ]);
        let mut failures = Vec::new();
        for filler in ["x", "\u{1f600}"] {
            for (kind, credential, marker) in probe_credentials() {
                for &offset in &offsets {
                    let mut source = prefix.clone();
                    while source.len() + filler.len() <= offset {
                        source.push_str(filler);
                    }
                    source.push(' ');
                    source.push_str(&credential);
                    source.push(' ');
                    source.push_str(&"y".repeat(20 * 1024));
                    let side = SessionCallDetailText::bounded(&source);
                    if side.text().contains(marker) {
                        failures.push(format!("{kind} filler={filler} offset={offset}"));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "leaks: {failures:?}");
    }

    #[test]
    fn probe_unparseable_arguments_with_escaped_quotes_leak_a_secret() -> TestResult {
        // Same argument text; the first parses, the second lacks its closing
        // brace (a malformed model call that canonical history keeps raw).
        let parsed = r#"{"cmd":"export API_TOKEN=\"hunter2hunter2\""}"#;
        let raw = r#"{"cmd":"export API_TOKEN=\"hunter2hunter2\""#;
        let history = CanonicalHistory::new(vec![
            exchange(20, parsed, &["ok"])?,
            exchange(21, raw, &["invalid arguments"])?,
        ])?;
        let parsed = found(project_call_detail(&history, call_id(20)?))?;
        assert!(
            !parsed.input().text().contains("hunter2"),
            "parsed path leaked"
        );
        let raw = found(project_call_detail(&history, call_id(21)?))?;
        assert!(
            !raw.input().text().contains("hunter2"),
            "raw path leaked: {}",
            raw.input().text()
        );
        Ok(())
    }

    #[test]
    fn probe_credential_key_holding_an_array_leaks() -> TestResult {
        let history = CanonicalHistory::new(vec![exchange(
            22,
            r#"{"password":["hunter2hunter2"],"api_key":{"v":"hunter3hunter3"}}"#,
            &["ok"],
        )?])?;
        let detail = found(project_call_detail(&history, call_id(22)?))?;
        let text = detail.input().text();
        assert!(
            !text.contains("hunter2") && !text.contains("hunter3"),
            "leaked: {text}"
        );
        Ok(())
    }

    #[test]
    fn probe_parsed_input_drops_the_tail_past_the_redaction_window() -> TestResult {
        // Canonical arguments are semantic JSON: at most 32_768 nodes and a
        // 256 KiB raw string, so an 8 MiB array cannot be stored. Thirty
        // thousand six-digit numbers fit those caps and pretty-print past the
        // 272 KiB window, so the last number must not come back.
        let raw = format!(r#"{{"v":[{}999999]}}"#, "123456,".repeat(30_000));
        let history = CanonicalHistory::new(vec![exchange(23, &raw, &["ok"])?])?;
        let detail = found(project_call_detail(&history, call_id(23)?))?;
        let text = detail.input().text();
        assert!(detail.input().truncated(), "len {}", text.len());
        assert!(!text.contains("999999"), "tail survived the window");
        assert!(text.len() <= MAX_CALL_DETAIL_SIDE_BYTES);
        Ok(())
    }
}
