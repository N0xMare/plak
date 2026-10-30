use super::*;

#[test]
fn meter_unwired() {
    assert_eq!(format_context_meter(None, None), "awaiting context");
    assert_eq!(format_context_meter(Some(0), None), "awaiting context");
    assert_eq!(format_context_meter(None, Some(272_000)), "0%/272k");
}

#[test]
fn meter_is_percent_over_public_window() {
    assert_eq!(format_context_meter(Some(0), Some(272_000)), "0%/272k");
    assert_eq!(format_context_meter(Some(15), Some(272_000)), "15%/272k");
    assert_eq!(format_context_meter(Some(99), Some(272_000)), "99%/272k");
    assert_eq!(format_context_meter(Some(1), Some(1_000_000)), "1%/1M");
    assert_eq!(format_context_meter(Some(50), Some(128_000)), "50%/128k");
    assert_eq!(format_window_label(258_400), "258.4k");
}

#[test]
fn wrap_cache_reuses_rows_until_text_or_width_changes() {
    let mut cache = DraftWrapCache::default();
    let first = cache.rows("hello world", 5).to_vec();
    assert_eq!(first, cache.rows("hello world", 5));
    assert_eq!(
        wrap_draft_lines("hello world", 5),
        first
            .iter()
            .map(|(_, _, line)| line.clone())
            .collect::<Vec<_>>()
    );
    let changed = cache.rows("hello world", 8).to_vec();
    assert_ne!(first, changed);
    let edited = cache.rows("hello worlds", 8).to_vec();
    assert_ne!(changed, edited);
}

#[test]
fn visual_cursor_survives_shift_enter_newline() {
    let text = "hello\nworld";
    // cursor after "hello\n" = 6
    let (col, row) = visual_cursor_xy(text, 40, 6);
    assert_eq!(row, 1);
    assert_eq!(col, 0);
    // click start of second line
    assert_eq!(cursor_from_visual_click(text, 40, 1, 0), 6);
    // click after "wo"
    assert_eq!(cursor_from_visual_click(text, 40, 1, 2), 8);
}

#[test]
fn unicode_rows_clicks_and_cursor_use_terminal_cells_and_graphemes() {
    assert_eq!(
        visual_rows("a界b", 2),
        vec![
            (0, 1, "a".to_owned()),
            (1, 2, "界".to_owned()),
            (2, 3, "b".to_owned())
        ]
    );
    assert_eq!(cursor_from_visual_click("a界b", 2, 1, 0), 1);
    assert_eq!(cursor_from_visual_click("a界b", 2, 1, 1), 1);
    assert_eq!(visual_cursor_xy("a界b", 2, 2), (0, 2));

    let combining = "e\u{301}x";
    assert_eq!(next_grapheme_boundary(combining, 0), 2);
    assert_eq!(previous_grapheme_boundary(combining, 2), 0);
    assert_eq!(visual_cursor_xy(combining, 8, 2), (1, 0));
}

#[test]
fn word_wrap_rows_are_shared_by_paint_cursor_and_hit_geometry() {
    let text = "go e\u{301}界z";
    assert_eq!(
        visual_rows(text, 4),
        vec![
            (0, 3, "go ".to_owned()),
            (3, 7, "e\u{301}界z".to_owned()),
            (7, 7, String::new())
        ]
    );
    assert_eq!(wrap_draft_lines(text, 4), ["go ", "e\u{301}界z", ""]);
    assert_eq!(visual_cursor_xy(text, 4, 3), (0, 1));
    assert_eq!(visual_cursor_xy(text, 4, 5), (1, 1));
    assert_eq!(cursor_from_visual_click(text, 4, 1, 2), 5);
    assert_eq!(cursor_from_visual_click(text, 4, 0, 3), 3);
}

#[test]
fn exact_width_end_caret_wraps_instead_of_reaching_the_border() {
    assert_eq!(visual_cursor_xy("abcd", 4, 4), (0, 1));
    assert_eq!(visual_line_count("abcd", 4), 2);
    assert_eq!(cursor_from_visual_click("abcd", 4, 1, 0), 4);
}

#[test]
fn height_has_two_content_rows_and_clamps_near_thirty_five_percent() {
    let h = desired_block_height(30, 40, "hi");
    assert_eq!(h, 4);
    assert_eq!(desired_block_height(30, 40, ""), 4);
    let tall = "x\n".repeat(50);
    let h2 = desired_block_height(30, 40, &tall);
    assert_eq!(h2, 11);
    assert_eq!(desired_block_height(40, 40, &tall), 14);
    assert_eq!(desired_block_height(5, 40, &tall), 4);
}

#[test]
fn word_boundaries_skip_separators_then_the_word() {
    let text = "alpha  beta_2 gamma";
    assert_eq!(previous_word_boundary(text, text.chars().count()), 14);
    assert_eq!(previous_word_boundary(text, 14), 7);
    assert_eq!(previous_word_boundary(text, 7), 0);
    assert_eq!(previous_word_boundary(text, 0), 0);
    assert_eq!(next_word_boundary(text, 0), 5);
    assert_eq!(next_word_boundary(text, 5), 13);
    assert_eq!(next_word_boundary(text, 13), 19);
    assert_eq!(next_word_boundary(text, 19), 19);
}

#[test]
fn word_boundaries_are_char_indexed_over_multibyte_text() {
    let text = "héllo wörld";
    assert_eq!(next_word_boundary(text, 0), 5);
    assert_eq!(previous_word_boundary(text, 11), 6);
}

#[test]
fn logical_line_bounds_stop_at_newlines() {
    let text = "first line\nsecond\nthird";
    assert_eq!(logical_line_start(text, 3), 0);
    assert_eq!(logical_line_end(text, 3), 10);
    assert_eq!(logical_line_start(text, 14), 11);
    assert_eq!(logical_line_end(text, 14), 17);
    assert_eq!(logical_line_end(text, 20), 23);
}
