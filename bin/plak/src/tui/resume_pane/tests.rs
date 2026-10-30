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
