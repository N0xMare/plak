use super::settings_registry::{ROWS, SettingKind, THEME_CHOICES, index_of, meta};
use crate::tui::theme::ThemeId;

#[test]
fn rows_are_unique_and_in_the_specified_order() {
    let labels: Vec<&str> = ROWS.iter().map(|row| row.label).collect();
    assert_eq!(
        labels,
        vec![
            "Theme",
            "Mouse capture",
            "Scroll lines",
            "Show thinking blocks",
            "Group tool calls",
            "Auto compaction",
            "Reduced motion",
            "Edit raw config",
        ]
    );
    for (index, row) in ROWS.iter().enumerate() {
        assert_eq!(index_of(row.key), Some(index), "duplicate key {}", row.key);
    }
}

#[test]
fn no_row_only_navigates_to_another_tab() {
    for row in ROWS {
        let label = row.label.to_ascii_lowercase();
        assert!(
            !label.starts_with("open "),
            "spec 02 §5.5 deletes navigation-only rows: {}",
            row.label
        );
    }
}

#[test]
fn theme_choices_match_the_shipped_theme_set() {
    let shipped: Vec<&str> = ThemeId::ALL.iter().map(|id| id.as_str()).collect();
    let registered: Vec<&str> = THEME_CHOICES.iter().map(|choice| choice.value).collect();
    assert_eq!(registered, shipped);
    assert_eq!(shipped.len(), 6, "spec 02 §5.5 fixes the theme set at six");
    assert!(shipped.contains(&"default-light"));
}

/// Identity-bearing bindings are not editable in Settings. Composition
/// and the `OpenRouter` wire are config-file only; the Codex processing tier
/// is a Models-tab extra on the exact model that admits it.
#[test]
fn no_row_binds_provider_identity() {
    let identity: Vec<&str> = ROWS
        .iter()
        .filter(|row| row.new_sessions)
        .map(|row| row.label)
        .collect();
    assert!(identity.is_empty(), "{identity:?}");
}

/// Settings apply on change, so there is no Save row and no
/// draft to commit.
#[test]
fn the_only_action_row_is_the_raw_config_editor() {
    let actions: Vec<&str> = ROWS
        .iter()
        .filter(|row| matches!(row.kind, SettingKind::Action))
        .map(|row| row.label)
        .collect();
    assert_eq!(actions, vec!["Edit raw config"]);
}

#[test]
fn only_the_theme_row_previews_live() {
    let previewing: Vec<&str> = ROWS
        .iter()
        .filter(|row| row.previews())
        .map(|row| row.label)
        .collect();
    assert_eq!(previewing, vec!["Theme"]);
}

#[test]
fn scroll_lines_is_bounded_by_the_spec_range() {
    let SettingKind::Int { min, max, default } =
        meta(super::settings_registry::SCROLL_LINES).unwrap().kind
    else {
        panic!("scroll lines must be an Int row");
    };
    assert_eq!((min, max), (1, 10));
    assert!((min..=max).contains(&default));
}

/// `tui.reading_width` was deleted: the row previewed,
/// persisted and wrapped nothing. No row may name it again.
#[test]
fn no_row_names_the_deleted_reading_width_key() {
    assert!(ROWS.iter().all(|row| row.key != "tui.reading_width"));
}

#[test]
fn every_row_carries_a_description_and_search_terms() {
    for row in ROWS {
        assert!(
            !row.description.is_empty(),
            "{} has no description",
            row.key
        );
        assert!(
            row.description.ends_with('.'),
            "{} is not a sentence",
            row.key
        );
        assert!(!row.keywords.is_empty(), "{} has no keywords", row.key);
        assert!(
            row.matches(row.label),
            "{} does not match its own label",
            row.key
        );
    }
}
