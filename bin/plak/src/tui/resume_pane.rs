//! Cwd-scoped durable-session picker state.
//!
//! The host owns discovery and supplies only sessions already filtered to the
//! current workspace. Selection returns the exact durable session id; the TUI
//! never resolves a display index or abbreviated id.
//!
//! One row is one session: its short id, how long ago it was last touched, the
//! first thing the operator asked it, and the binding it comes back under.
//! Rows are ordered newest first by the host and drawn through the shared
//! Menu row renderer (spec 02 §5.5), so the picker is the same object as
//! every other list in the product.

use crate::tui::ResumeSessionEntry;
use crate::tui::text_input::is_accepted_single_line_char;

/// Characters of the durable id a row shows. Eight is enough to tell one
/// workspace's sessions apart; the exact id is what Enter resumes, and it never
/// has to be read off the row or typed.
pub const SHORT_ID_CHARS: usize = 8;

/// Columns the age column occupies, so ids, ages and summaries line up down the
/// list. `yesterday` is 9 and an exact date is 10.
pub const AGE_COLUMN_WIDTH: usize = 10;

/// Summary shown for a session that was never prompted and carries no title.
pub const NO_PROMPT_SUMMARY: &str = "(no prompt yet)";

/// Value shown for a session that recorded neither account nor model.
pub const UNBOUND_BINDING: &str = "unbound";

/// Row width at or above which the value column names both the account and the
/// model. See [`ResumeSessionEntry::display_binding_for_width`].
pub const WIDE_BINDING_ROW_WIDTH: u16 = 72;

/// Characters of prompt or title a summary keeps, matching the host's own
/// projection bound.
const MAX_SUMMARY_CHARS: usize = 200;

const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
/// Age at or beyond which a row shows an exact date rather than a distance:
/// past a week, `9d ago` stops being a day the operator can place.
const RELATIVE_AGE_HORIZON: u64 = 8 * SECONDS_PER_DAY;

/// Row wording for the resume picker, derived from host-projected fields.
pub trait ResumeRow {
    /// Bounded, single-line display of the opaque host-validated identity.
    #[cfg(test)]
    fn display_id(&self) -> String;
    /// Leading [`SHORT_ID_CHARS`] of the sanitized id, as a row shows it.
    fn short_id(&self) -> String;
    /// Bounded, single-line account metadata safe for terminal list rows.
    fn display_account(&self) -> Option<String>;
    /// Bounded, single-line model metadata safe for terminal list rows.
    fn display_model(&self) -> Option<String>;
    /// Bounded, single-line effort metadata safe for terminal list rows.
    fn display_effort(&self) -> Option<String>;
    /// Bounded, single-line optional model control safe for terminal rows.
    fn display_extra(&self) -> Option<String>;
    /// A row's one-line summary: what the operator first asked this session,
    /// falling back to its recorded title and then to an explicit "nothing was
    /// asked yet" rather than an empty column.
    fn display_summary(&self) -> String;
    /// A row's right-aligned value: the account and model this session resumes
    /// under.
    fn display_binding(&self) -> String;
    /// The value column for a row `row_width` columns wide.
    ///
    /// Below `WIDE_BINDING_ROW_WIDTH` the account is dropped and the model
    /// stands alone: the summary is the only column that says what a session
    /// was, and a full `account · model` erases it on a narrow terminal.
    fn display_binding_for_width(&self, row_width: u16) -> String;
    /// Relative distance from `now_unix` back to this session's last update,
    /// with `zone` deciding the calendar day an old session falls on.
    fn display_age(&self, now_unix: u64, zone: &jiff::tz::TimeZone) -> String;
}

impl ResumeRow for ResumeSessionEntry {
    /// Bounded, single-line display of the opaque host-validated identity.
    #[cfg(test)]
    fn display_id(&self) -> String {
        safe_metadata(&self.id, 128)
    }

    /// Leading [`SHORT_ID_CHARS`] of the sanitized id, as a row shows it.
    fn short_id(&self) -> String {
        safe_metadata(&self.id, SHORT_ID_CHARS)
            .trim_end_matches('…')
            .to_owned()
    }

    /// Bounded, single-line account metadata safe for terminal list rows.
    fn display_account(&self) -> Option<String> {
        self.account
            .as_deref()
            .map(|value| safe_metadata(value, 96))
    }

    /// Bounded, single-line model metadata safe for terminal list rows.
    fn display_model(&self) -> Option<String> {
        self.model.as_deref().map(|value| safe_metadata(value, 128))
    }

    /// Bounded, single-line effort metadata safe for terminal list rows.
    fn display_effort(&self) -> Option<String> {
        self.effort.as_deref().map(|value| safe_metadata(value, 48))
    }

    /// Bounded, single-line optional model control safe for terminal rows.
    fn display_extra(&self) -> Option<String> {
        self.extra.as_deref().map(|value| safe_metadata(value, 48))
    }

    /// A row's one-line summary: what the operator first asked this session,
    /// falling back to its recorded title and then to an explicit "nothing was
    /// asked yet" rather than an empty column.
    fn display_summary(&self) -> String {
        self.first_prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                self.title
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
            .map_or_else(
                || NO_PROMPT_SUMMARY.to_owned(),
                |value| safe_metadata(value, MAX_SUMMARY_CHARS),
            )
    }

    /// A row's right-aligned value: the account and model this session resumes
    /// under.
    fn display_binding(&self) -> String {
        match (self.display_account(), self.display_model()) {
            (Some(account), Some(model)) => format!("{account} · {model}"),
            (Some(only), None) | (None, Some(only)) => only,
            (None, None) => UNBOUND_BINDING.to_owned(),
        }
    }

    /// The value column for a row `row_width` columns wide.
    ///
    /// Below [`WIDE_BINDING_ROW_WIDTH`] the account is dropped and the model
    /// stands alone: the summary is the only column that says what a session
    /// was, and a full `account · model` erases it on a narrow terminal.
    fn display_binding_for_width(&self, row_width: u16) -> String {
        if row_width >= WIDE_BINDING_ROW_WIDTH {
            return self.display_binding();
        }
        self.display_model()
            .or_else(|| self.display_account())
            .unwrap_or_else(|| UNBOUND_BINDING.to_owned())
    }

    /// Relative distance from `now_unix` back to this session's last update,
    /// with `zone` deciding the calendar day an old session falls on.
    fn display_age(&self, now_unix: u64, zone: &jiff::tz::TimeZone) -> String {
        relative_age(self.updated_at_unix, now_unix, zone)
    }
}

/// How long ago `then_unix` was, as the picker says it.
///
/// A session stamped in the future — a restored backup, a stepped host clock —
/// reads as `just now` rather than a negative distance.
#[must_use]
pub fn relative_age(then_unix: u64, now_unix: u64, zone: &jiff::tz::TimeZone) -> String {
    let elapsed = now_unix.saturating_sub(then_unix);
    if elapsed < SECONDS_PER_MINUTE {
        return "just now".to_owned();
    }
    if elapsed < SECONDS_PER_HOUR {
        return format!("{}m ago", elapsed / SECONDS_PER_MINUTE);
    }
    if elapsed < SECONDS_PER_DAY {
        return format!("{}h ago", elapsed / SECONDS_PER_HOUR);
    }
    if elapsed < 2 * SECONDS_PER_DAY {
        return "yesterday".to_owned();
    }
    if elapsed < RELATIVE_AGE_HORIZON {
        return format!("{}d ago", elapsed / SECONDS_PER_DAY);
    }
    calendar_date(then_unix, zone)
}

/// `YYYY-MM-DD` for `unix_seconds` in `zone` (the launch's).
///
/// Local wall clock, not UTC: the Usage tab already prints local instants on
/// the same screen, and an evening operator was shown sessions that "started
/// tomorrow".
#[must_use]
pub fn calendar_date(unix_seconds: u64, zone: &jiff::tz::TimeZone) -> String {
    i64::try_from(unix_seconds)
        .ok()
        .and_then(|seconds| jiff::Timestamp::from_second(seconds).ok())
        .map_or_else(
            || "unknown date".to_owned(),
            |moment| moment.to_zoned(zone.clone()).date().to_string(),
        )
}

fn safe_metadata(value: &str, maximum_chars: usize) -> String {
    let mut rendered = String::new();
    let mut truncated = false;
    for (count, character) in value.chars().enumerate() {
        if count == maximum_chars {
            truncated = true;
            break;
        }
        if is_accepted_single_line_char(character) {
            rendered.push(character);
        } else {
            rendered.push(' ');
        }
    }
    if truncated {
        rendered.push('…');
    }
    rendered
}

/// Terminal result of the shared host-backed picker state machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumePickerOutcome {
    /// The host snapshot contained no resumable session.
    Empty,
    /// The operator dismissed the picker without changing sessions.
    #[cfg(test)]
    Cancelled,
    /// The operator selected one exact, canonical product-session UUID.
    Selected(ResumeSessionEntry),
}

/// Interactive cwd-scoped resume picker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeMenu {
    /// Most-recent-first rows supplied by the host.
    pub entries: Vec<ResumeSessionEntry>,
    /// Selected row.
    pub selected: usize,
    /// Wall clock the ages are measured from, captured once when the picker
    /// opened. Every row on a frame therefore reads against one instant, and
    /// rendering stays a pure function of this state.
    pub now_unix: u64,
    /// The launch's zone, so every date this picker prints is the
    /// operator's own wall clock.
    pub zone: jiff::tz::TimeZone,
}

impl ResumeMenu {
    /// Open over one cwd-scoped snapshot, measuring ages from `now_unix` and
    /// reading calendar dates in `zone`.
    #[must_use]
    pub fn open(entries: Vec<ResumeSessionEntry>, now_unix: u64, zone: jiff::tz::TimeZone) -> Self {
        Self {
            entries,
            selected: 0,
            now_unix,
            zone,
        }
    }

    /// The calendar day one row's session started, on the operator's clock.
    #[must_use]
    pub fn started_date(&self, entry: &ResumeSessionEntry) -> String {
        calendar_date(entry.created_at_unix, &self.zone)
    }

    /// Resolve Enter using the exact same semantics for launch and in-app use.
    #[must_use]
    pub fn accept(&self) -> ResumePickerOutcome {
        self.selected_entry()
            .cloned()
            .map_or(ResumePickerOutcome::Empty, ResumePickerOutcome::Selected)
    }

    /// Resolve explicit dismissal without inventing a selection.
    #[must_use]
    #[cfg(test)]
    pub const fn cancel() -> ResumePickerOutcome {
        ResumePickerOutcome::Cancelled
    }

    /// Move selection, wrapping at both ends.
    pub fn move_selection(&mut self, delta: i8) {
        let len = self.entries.len();
        if len == 0 {
            return;
        }
        let current = self.selected.min(len - 1);
        self.selected = if delta < 0 {
            if current == 0 { len - 1 } else { current - 1 }
        } else {
            (current + 1) % len
        };
    }

    /// Exact id of the selected durable session.
    #[must_use]
    #[cfg(test)]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_entry().map(|entry| entry.id.as_str())
    }

    /// Complete selected row for exact binding comparison.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ResumeSessionEntry> {
        self.entries
            .get(self.selected.min(self.entries.len().saturating_sub(1)))
    }

    /// The label of one row, left of its right-aligned binding: short id, age,
    /// and the session's opening prompt. The renderer clips the tail, so a long
    /// prompt loses its end and never the identity that names the row.
    #[must_use]
    pub fn row_label(&self, index: usize) -> Option<String> {
        let entry = self.entries.get(index)?;
        let age = entry.display_age(self.now_unix, &self.zone);
        Some(format!(
            "{:<id_width$}  {age:<age_width$}  {}",
            entry.short_id(),
            entry.display_summary(),
            id_width = SHORT_ID_CHARS,
            age_width = AGE_COLUMN_WIDTH,
        ))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod tests {
    use super::*;

    /// 2026-09-03T12:00:00Z, the instant every age case is measured against.
    const NOW: u64 = 1_788_436_800;
    const UTC: jiff::tz::TimeZone = jiff::tz::TimeZone::UTC;

    fn entry(id: &str) -> ResumeSessionEntry {
        ResumeSessionEntry {
            id: id.to_owned(),
            ..ResumeSessionEntry::default()
        }
    }

    #[test]
    fn selection_wraps_and_preserves_exact_id() {
        let exact = "018f2537-5ed2-7b11-a4b1-f009be85f101";
        let mut menu = ResumeMenu::open(vec![entry(exact), entry("other")], NOW, UTC);
        assert_eq!(menu.selected_id(), Some(exact));
        menu.move_selection(-1);
        assert_eq!(menu.selected_id(), Some("other"));
        menu.move_selection(1);
        assert_eq!(menu.selected_id(), Some(exact));
    }

    #[test]
    fn empty_snapshot_has_no_selection() {
        let mut menu = ResumeMenu::open(Vec::new(), NOW, UTC);
        menu.move_selection(1);
        assert_eq!(menu.selected_id(), None);
        assert_eq!(menu.accept(), ResumePickerOutcome::Empty);
        assert_eq!(ResumeMenu::cancel(), ResumePickerOutcome::Cancelled);
    }

    #[test]
    fn exact_identity_and_metadata_are_safe_for_host_backed_picker_rows() {
        let exact = "018f2537-5ed2-7b11-a4b1-f009be85f101";
        let mut row = entry(exact);
        row.account = Some("account\nspoof".into());
        row.first_prompt = Some(format!("{}\u{1b}[31m", "p".repeat(241)));
        let menu = ResumeMenu::open(vec![row.clone()], NOW, UTC);
        assert_eq!(menu.accept(), ResumePickerOutcome::Selected(row.clone()));
        assert_eq!(row.display_account().as_deref(), Some("account spoof"));
        assert!(!row.display_summary().contains('\u{1b}'));
        assert!(row.display_summary().ends_with('…'));
        row.id = "id\u{202e}spoof".into();
        assert!(!row.display_id().contains('\u{202e}'));
    }

    /// The one table the age column is judged against: every branch, and both
    /// sides of every boundary between them.
    #[test]
    fn relative_age_names_every_distance_the_picker_shows() {
        let cases: [(u64, &str); 14] = [
            (0, "just now"),
            (1, "just now"),
            (59, "just now"),
            (60, "1m ago"),
            (120, "2m ago"),
            (59 * 60 + 59, "59m ago"),
            (60 * 60, "1h ago"),
            (3 * 60 * 60, "3h ago"),
            (24 * 60 * 60 - 1, "23h ago"),
            (24 * 60 * 60, "yesterday"),
            (2 * 24 * 60 * 60 - 1, "yesterday"),
            (2 * 24 * 60 * 60, "2d ago"),
            (4 * 24 * 60 * 60, "4d ago"),
            (7 * 24 * 60 * 60, "7d ago"),
        ];
        for (elapsed, expected) in cases {
            assert_eq!(
                relative_age(NOW - elapsed, NOW, &UTC),
                expected,
                "{elapsed}s before now"
            );
        }
        // Past a week the distance stops being placeable and the row shows the
        // exact day instead.
        assert_eq!(
            relative_age(NOW - 8 * 24 * 60 * 60, NOW, &UTC),
            "2026-08-26"
        );
        assert_eq!(
            relative_age(NOW - 400 * 24 * 60 * 60, NOW, &UTC),
            "2025-07-30"
        );
        // A session stamped in the future never renders a negative distance.
        assert_eq!(relative_age(NOW + 90, NOW, &UTC), "just now");
    }

    #[test]
    fn calendar_dates_are_civil_days_at_the_offset_they_are_given() {
        assert_eq!(calendar_date(0, &UTC), "1970-01-01");
        assert_eq!(calendar_date(951_782_400, &UTC), "2000-02-29");
        assert_eq!(calendar_date(NOW, &UTC), "2026-09-03");
        assert_eq!(calendar_date(u64::MAX, &UTC), "unknown date");
    }

    /// The picker's dates are the operator's wall clock.
    ///
    /// `NOW` is 2026-09-03 12:00 UTC; four hours before midnight UTC the same
    /// instant is still the 3rd in New York and already the 4th in Berlin. A
    /// session opened seconds ago used to read `started 2026-09-04` for an
    /// evening operator in the Americas, which is a date they cannot place.
    #[test]
    fn a_started_date_is_read_on_the_operators_clock_not_in_utc() {
        const EVENING_UTC: u64 = NOW + 8 * 60 * 60; // 2026-09-03 20:00 UTC
        const NEW_YORK: jiff::tz::TimeZone = jiff::tz::TimeZone::fixed(jiff::tz::offset(-4));
        const BERLIN: jiff::tz::TimeZone = jiff::tz::TimeZone::fixed(jiff::tz::offset(2));
        // 2026-09-04 00:10 UTC, just after UTC midnight.
        const AFTER_UTC_MIDNIGHT: u64 = NOW + 12 * 60 * 60 + 10 * 60;

        assert_eq!(calendar_date(EVENING_UTC, &UTC), "2026-09-03");
        assert_eq!(calendar_date(EVENING_UTC, &NEW_YORK), "2026-09-03");
        assert_eq!(calendar_date(EVENING_UTC, &BERLIN), "2026-09-03");

        assert_eq!(calendar_date(AFTER_UTC_MIDNIGHT, &UTC), "2026-09-04");
        assert_eq!(
            calendar_date(AFTER_UTC_MIDNIGHT, &NEW_YORK),
            "2026-09-03",
            "a session opened at 20:10 local must not read as started tomorrow"
        );

        let mut row = entry("018f2537-5ed2-7b11-a4b1-f009be85f101");
        row.created_at_unix = AFTER_UTC_MIDNIGHT;
        row.updated_at_unix = AFTER_UTC_MIDNIGHT;
        let menu = ResumeMenu::open(vec![row.clone()], AFTER_UTC_MIDNIGHT, NEW_YORK);
        assert_eq!(menu.started_date(&row), "2026-09-03");
        // An age past the relative horizon is a date, and it is local too.
        assert_eq!(
            row.display_age(AFTER_UTC_MIDNIGHT + 30 * 24 * 60 * 60, &NEW_YORK),
            "2026-09-03"
        );
    }

    #[test]
    fn a_row_label_puts_identity_first_and_the_summary_last() {
        let mut row = entry("018f2537-5ed2-7b11-a4b1-f009be85f101");
        row.updated_at_unix = NOW - 3 * 60 * 60;
        row.first_prompt = Some("Rewrite the resume picker".into());
        let menu = ResumeMenu::open(vec![row], NOW, UTC);
        assert_eq!(
            menu.row_label(0).as_deref(),
            Some("018f2537  3h ago      Rewrite the resume picker")
        );
        assert!(menu.row_label(1).is_none());
    }

    #[test]
    fn a_summary_falls_back_to_the_title_then_to_an_explicit_absence() {
        let mut row = entry("id");
        assert_eq!(row.display_summary(), NO_PROMPT_SUMMARY);
        row.title = Some("  Nightly triage  ".into());
        assert_eq!(row.display_summary(), "Nightly triage");
        row.first_prompt = Some("   ".into());
        assert_eq!(
            row.display_summary(),
            "Nightly triage",
            "a blank prompt is not a summary"
        );
        row.first_prompt = Some("What broke?".into());
        assert_eq!(row.display_summary(), "What broke?");
    }

    #[test]
    fn the_binding_column_names_whichever_half_the_session_recorded() {
        let mut row = entry("id");
        assert_eq!(row.display_binding(), UNBOUND_BINDING);
        row.model = Some("grok-4.6".into());
        assert_eq!(row.display_binding(), "grok-4.6");
        row.account = Some("xai".into());
        assert_eq!(row.display_binding(), "xai · grok-4.6");
        row.model = None;
        assert_eq!(row.display_binding(), "xai");
    }

    #[test]
    fn a_narrow_row_keeps_the_model_and_gives_up_the_account() {
        let mut row = entry("id");
        row.account = Some("anthropic".into());
        row.model = Some("claude-opus-5".into());
        assert_eq!(
            row.display_binding_for_width(WIDE_BINDING_ROW_WIDTH),
            "anthropic · claude-opus-5"
        );
        assert_eq!(
            row.display_binding_for_width(WIDE_BINDING_ROW_WIDTH - 1),
            "claude-opus-5"
        );
        row.model = None;
        assert_eq!(row.display_binding_for_width(20), "anthropic");
        row.account = None;
        assert_eq!(row.display_binding_for_width(20), UNBOUND_BINDING);
    }

    #[test]
    fn a_short_id_is_eight_characters_and_carries_no_clip_mark() {
        let row = entry("018f2537-5ed2-7b11-a4b1-f009be85f101");
        assert_eq!(row.short_id(), "018f2537");
        assert_eq!(entry("abc").short_id(), "abc");
    }
}
