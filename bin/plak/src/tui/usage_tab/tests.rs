#[test]
fn wrapped_limit_reset_rows_keep_the_source_indent() {
    use ratatui::text::{Line, Span};
    for indent in [2, 4] {
        let source = format!("{}(resets Tue 23:44 local time)", " ".repeat(indent));
        let rows = super::fit_limits_lines(vec![Line::from(Span::raw(source))], 18);
        assert!(rows.len() > 1);
        for row in rows {
            assert!(row.width() <= 18);
            let text = row
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(text.starts_with(&" ".repeat(indent)), "{text:?}");
        }
    }
}

use super::{
    CONTEXT_GRID_CELLS, GRID_OCCUPIED, GRID_RESERVED, LIMIT_BAR_CELL, LIMIT_BAR_CELLS,
    LimitsChrome, SessionLive, SessionRow, cells_for, context_lines, fmt_tok, is_context_grid_line,
    limits_lines, occupying_categories, percent_left_of, percent_of_window, reserved_and_free,
    session_copy_text, session_ledger_lines, session_lines, session_rows,
};
use crate::tui::presentation::{
    AdmittedContextCeiling, ContextBreakdownView, ContextCategoryView, ContextInfoRowView,
    ContextUsage, PreparedInputUsage, SessionInfoView, SessionLedgerView, UsageAmountView,
    UsageCostView, UsageLimitsView, UsageMeterReading, UsageMeterView, UsageVertical,
    VerticalUsageState, VerticalUsageView,
};
use crate::tui::theme::Theme;
use ratatui::style::Style;

fn chrome(fence: Option<u8>) -> super::ContextChrome<'static> {
    super::ContextChrome {
        model_label: Some("grok-4.6"),
        auto_compact_enabled: fence.is_some(),
    }
}

fn usage(used: u64, total: u64, fence: Option<u8>) -> ContextUsage {
    ContextUsage {
        breakdown: Some(ContextBreakdownView {
            used_tokens: used,
            total_tokens: total,
            categories: vec![
                ContextCategoryView {
                    label: "System".to_owned(),
                    tokens: used / 10,
                },
                ContextCategoryView {
                    label: "Messages".to_owned(),
                    tokens: used - used / 10,
                },
            ],
            informational: vec![ContextInfoRowView {
                label: "Tool definitions".to_owned(),
                tokens: 5_600,
                detail: Some("12 tools".to_owned()),
            }],
            auto_compact_trigger_tokens: fence
                .map(|pct| total.saturating_mul(u64::from(pct)) / 100),
            occupancy_anchored: true,
            turns: Some(5),
            tool_calls: Some(12),
            compactions: Some(0),
        }),
        prepared_input: Some(PreparedInputUsage::Exact(used)),
        total_window: Some(total),
        admitted_ceiling: Some(AdmittedContextCeiling {
            tokens: total,
            basis: "exact".to_owned(),
        }),
        gauge_percent: None,
        display_percent: None,
        ceiling_excess: None,
        evidence: "release-verified".to_owned(),
        non_guarantee: None,
        status: "available".to_owned(),
    }
}

fn text_of(lines: &[ratatui::text::Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

fn grid_only(lines: &[String], width: u16) -> Vec<String> {
    let per_row: usize = if width <= super::NARROW_BREAKPOINT {
        10
    } else {
        20
    };
    let n = per_row.saturating_mul(2).saturating_sub(1);
    lines
        .iter()
        .filter(|line| is_context_grid_line(line, width))
        .map(|line| line.chars().take(n).collect())
        .collect()
}

#[test]
fn token_formatting_avoids_the_width_discontinuity() {
    assert_eq!(fmt_tok(0), "0");
    assert_eq!(fmt_tok(999), "999");
    assert_eq!(fmt_tok(1_000), "1.0k");
    assert_eq!(fmt_tok(36_700), "36.7k");
    assert_eq!(fmt_tok(99_499), "99.5k");
    assert_eq!(fmt_tok(99_500), "100k");
    assert_eq!(fmt_tok(963_000), "963k");
    assert_eq!(fmt_tok(1_000_000), "1.0m");
}

#[test]
fn tiny_nonzero_shares_floor_to_a_tenth_of_a_percent() {
    assert_eq!(percent_of_window(0, 1_000), "0.0%");
    assert_eq!(percent_of_window(1, 1_000_000), "0.1%");
    assert_eq!(percent_of_window(30_000, 1_000_000), "3.0%");
    assert_eq!(percent_of_window(960_000, 1_000_000), "96%");
    assert_eq!(percent_of_window(5, 0), "—");
}

#[test]
fn the_grid_is_always_exactly_one_hundred_cells_at_both_breakpoints() {
    let theme = Theme::dark();
    for (width, rows, per_row) in [(120_u16, 5_usize, 20_usize), (40, 10, 10)] {
        let lines = text_of(&context_lines(
            &usage(36_700, 1_000_000, Some(85)),
            &chrome(Some(85)),
            width,
            &theme,
        ));
        let grid = grid_only(&lines, width);
        assert_eq!(grid.len(), rows, "width {width} grid row count");
        let cells: usize = grid
            .iter()
            .map(|line| line.chars().filter(|ch| *ch != ' ').count())
            .sum();
        assert_eq!(cells, CONTEXT_GRID_CELLS, "width {width} grid cell count");
        for line in &grid {
            assert_eq!(line.chars().filter(|ch| *ch != ' ').count(), per_row);
        }
    }
}

#[test]
fn cells_round_from_the_share() {
    assert_eq!(cells_for(0, 1_000), 0);
    assert_eq!(cells_for(4, 1_000), 0);
    assert_eq!(cells_for(5, 1_000), 1);
    assert_eq!(cells_for(1_000, 1_000), 100);
}

#[test]
fn the_reserved_tail_is_the_share_past_the_fence() {
    assert_eq!(
        reserved_and_free(36_700, 1_000_000, Some(850_000)),
        (813_300, 150_000)
    );
    assert_eq!(
        reserved_and_free(900_000, 1_000_000, Some(850_000)),
        (0, 100_000)
    );
    assert_eq!(reserved_and_free(36_700, 1_000_000, None), (963_300, 0));
}

#[test]
fn the_autocompact_buffer_occupies_the_unused_tail() {
    let theme = Theme::dark();
    let lines = text_of(&context_lines(
        &usage(36_700, 1_000_000, Some(85)),
        &chrome(Some(85)),
        40,
        &theme,
    ));
    let cells: String = grid_only(&lines, 40)
        .iter()
        .flat_map(|line| line.chars().filter(|ch| *ch != ' '))
        .collect();
    assert_eq!(cells.chars().count(), CONTEXT_GRID_CELLS);
    let reserved: String = cells
        .chars()
        .rev()
        .take_while(|ch| *ch == GRID_RESERVED)
        .collect();
    assert_eq!(reserved.chars().count(), 15);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Autocompact buffer: 150k tokens"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Estimated usage by category"))
    );
}

#[test]
fn the_board_glyphs_are_one_cell_wide() {
    use unicode_width::UnicodeWidthChar;
    for glyph in [
        GRID_OCCUPIED,
        super::GRID_FREE,
        GRID_RESERVED,
        LIMIT_BAR_CELL,
    ] {
        assert_eq!(glyph.width(), Some(1), "U+{:04X}", u32::from(glyph));
    }
}

#[test]
fn a_breakdown_free_projection_still_draws_one_hundred_honest_cells() {
    let theme = Theme::dark();
    let mut bare = usage(250_000, 1_000_000, Some(85));
    bare.breakdown = None;
    let lines = text_of(&context_lines(&bare, &chrome(Some(85)), 120, &theme));
    let cells: usize = grid_only(&lines, 120)
        .iter()
        .map(|line| line.chars().filter(|ch| *ch != ' ').count())
        .sum();
    assert_eq!(cells, CONTEXT_GRID_CELLS);
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&format!("{GRID_OCCUPIED} Used:")))
    );
    assert!(!lines.iter().any(|line| line.contains('◈')));
    // A breakdown-free projection counts nothing, and says so.
    assert!(lines.iter().any(|line| {
        line == "Turns: not reported · Tool calls: not reported · Compactions: not reported"
    }));
}

/// The counter line is three facts, not one that gets clipped.
///
/// At 80x24 the Menu body is 66 columns and the joined line is
/// 74, so `Compactions: not` was the last thing the operator saw — cut
/// mid-word, with no ellipsis, because the tab's paragraph does not wrap.
#[test]
fn the_context_counters_stack_rather_than_clip_on_a_narrow_tab() {
    // The Menu body at 80x24.
    const NARROW: u16 = 66;

    let theme = Theme::dark();
    let mut bare = usage(250_000, 1_000_000, Some(85));
    bare.breakdown = None;

    let wide = text_of(&context_lines(&bare, &chrome(Some(85)), 120, &theme));
    assert!(wide.iter().any(|line| {
        line == "Turns: not reported · Tool calls: not reported · Compactions: not reported"
    }));

    let narrow = text_of(&context_lines(&bare, &chrome(Some(85)), NARROW, &theme));
    assert!(
        narrow
            .iter()
            .all(|line| line.chars().count() <= usize::from(NARROW)),
        "no Context line exceeds the body it is drawn into: {narrow:#?}"
    );
    for counter in [
        "Turns: not reported",
        "Tool calls: not reported",
        "Compactions: not reported",
    ] {
        assert!(
            narrow.iter().any(|line| line == counter),
            "`{counter}` survives whole: {narrow:#?}"
        );
    }
}

#[test]
fn the_tip_band_renders_only_between_eighty_and_the_fence() {
    let theme = Theme::dark();
    let tip = "Tip: run /compact to free up context space.";
    let below = text_of(&context_lines(
        &usage(700_000, 1_000_000, Some(85)),
        &chrome(Some(85)),
        120,
        &theme,
    ));
    assert!(!below.iter().any(|line| line == tip));
    let inside = text_of(&context_lines(
        &usage(820_000, 1_000_000, Some(85)),
        &chrome(Some(85)),
        120,
        &theme,
    ));
    assert!(inside.iter().any(|line| line == tip));
    assert!(
        inside
            .iter()
            .any(|line| line.starts_with("Auto-compact at 85% (850k)"))
    );
    let past = text_of(&context_lines(
        &usage(900_000, 1_000_000, Some(85)),
        &chrome(Some(85)),
        120,
        &theme,
    ));
    assert!(!past.iter().any(|line| line == tip), "tip never stacks");
    assert!(
        past.iter()
            .any(|line| line == "Auto-compact triggers next turn (at 85%)")
    );
}

/// Over the fence on an unanchored measure the idle fence will not fire,
/// so the footer must not promise a compaction.
#[test]
fn an_unanchored_measure_over_the_fence_does_not_promise_auto_compact() {
    let theme = Theme::dark();
    let mut over = usage(900_000, 1_000_000, Some(85));
    over.breakdown.as_mut().unwrap().occupancy_anchored = false;
    let lines = text_of(&context_lines(&over, &chrome(Some(85)), 120, &theme));
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("Auto-compact triggers"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "Over 85%: auto-compact waits for a provider token count")
    );
}

/// The auto-compact line names the fence the *host envelope* carries, in
/// tokens as well as percent: a 1M window fences at 850k.
#[test]
fn the_auto_compact_line_names_the_host_envelope_fence_in_tokens() {
    let theme = Theme::dark();
    let envelope = usage(36_700, 1_000_000, Some(85));
    // Chrome enables compaction but does not supply the threshold.
    let lines = text_of(&context_lines(&envelope, &chrome(Some(50)), 120, &theme));
    assert!(
        lines
            .iter()
            .any(|line| line == "Auto-compact at 85% (850k) · ~813k tokens remaining"),
        "{lines:?}"
    );
}

#[test]
fn enabled_compaction_without_a_reported_trigger_is_not_off() {
    for (breakdown_present, prepared_present) in [(true, true), (false, true), (false, false)] {
        let mut view = usage(36_700, 1_000_000, None);
        if !breakdown_present {
            view.breakdown = None;
        }
        if !prepared_present {
            view.prepared_input = None;
        }
        let lines = text_of(&context_lines(
            &view,
            &chrome(Some(90)),
            120,
            &Theme::dark(),
        ));
        assert!(
            lines.iter().any(|line| {
                line == "Auto compaction is on; trigger not reported for this route."
            })
        );
        assert!(!lines.iter().any(|line| line == "Auto compaction is off."));
    }
    let lines = text_of(&context_lines(
        &usage(36_700, 1_000_000, Some(90)),
        &chrome(None),
        120,
        &Theme::dark(),
    ));
    assert!(lines.iter().any(|line| line == "Auto compaction is off."));
}

#[test]
fn non_round_triggers_keep_exact_tokens_and_do_not_fire_at_the_rounded_percent() {
    let mut view = usage(151_000, 200_000, None);
    view.breakdown.as_mut().unwrap().auto_compact_trigger_tokens = Some(151_201);
    let lines = text_of(&context_lines(
        &view,
        &chrome(Some(90)),
        120,
        &Theme::dark(),
    ));
    assert!(
        lines
            .iter()
            .any(|line| { line == "Auto-compact at 76% (151201) · ~201 tokens remaining" }),
        "{lines:?}"
    );
    assert_eq!(
        reserved_and_free(151_000, 200_000, Some(151_201)),
        (201, 48_799)
    );

    view.breakdown.as_mut().unwrap().used_tokens = 151_201;
    let lines = text_of(&context_lines(
        &view,
        &chrome(Some(90)),
        120,
        &Theme::dark(),
    ));
    assert!(
        lines
            .iter()
            .any(|line| line == "Auto-compact triggers next turn (at 76%)")
    );
}

#[test]
fn the_header_percent_is_recomputed_not_read_from_a_rounded_field() {
    let theme = Theme::dark();
    let lines = text_of(&context_lines(
        &usage(36_700, 1_000_000, Some(85)),
        &chrome(Some(85)),
        120,
        &theme,
    ));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("36.7k / 1.0m tokens (3.67%)"))
    );
    assert!(lines.iter().any(|line| line.contains("grok-4.6")));
}

#[test]
fn occupancy_header_keeps_the_admission_bound_as_its_own_fact() {
    let theme = Theme::dark();
    let mut view = usage(4_000, 200_000, Some(85));
    view.prepared_input = Some(PreparedInputUsage::UpperBound(20_000));
    if let Some(breakdown) = view.breakdown.as_mut() {
        breakdown.used_tokens = 4_000;
        breakdown.informational = vec![ContextInfoRowView {
            label: "Admission padding (est.)".to_owned(),
            tokens: 16_000,
            detail: None,
        }];
    }
    let lines = text_of(&context_lines(&view, &chrome(Some(85)), 120, &theme));
    assert!(
        lines.iter().any(|line| line.contains("4.0k / 200k tokens")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("≤4.0k")),
        "occupancy must not wear the bound qualifier: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Prepared input ≤20.0k tokens")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Admission padding (est.)")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Auto-compact at 85% (170k) · ~166k tokens remaining")),
        "compact remaining uses occupancy, same as the header: {lines:?}"
    );
}

#[test]
fn admission_padding_does_not_enter_the_board() {
    let theme = Theme::dark();
    let mut view = usage(4_000, 200_000, None);
    view.prepared_input = Some(PreparedInputUsage::UpperBound(20_000));
    if let Some(breakdown) = view.breakdown.as_mut() {
        breakdown.used_tokens = 4_000;
        breakdown.categories = vec![
            ContextCategoryView {
                label: "System prompt (est.)".to_owned(),
                tokens: 1_000,
            },
            ContextCategoryView {
                label: "Tool definitions (est. · 7 tools)".to_owned(),
                tokens: 3_000,
            },
        ];
        breakdown.informational = vec![ContextInfoRowView {
            label: "Admission padding (est.)".to_owned(),
            tokens: 16_000,
            detail: None,
        }];
    }
    let lines = text_of(&context_lines(&view, &chrome(None), 120, &theme));
    let occupied: usize = grid_only(&lines, 120)
        .iter()
        .flat_map(|line| line.chars().filter(|ch| *ch == GRID_OCCUPIED))
        .count();
    assert!(
        occupied < 10,
        "padding must not fill the board: occupied={occupied} {lines:?}"
    );
}

#[test]
fn occupying_categories_keep_envelope_and_provider_floor_distinct() {
    let breakdown = ContextBreakdownView {
        used_tokens: 5_000,
        total_tokens: 200_000,
        categories: vec![
            ContextCategoryView {
                label: "System prompt (est.)".to_owned(),
                tokens: 10,
            },
            ContextCategoryView {
                label: "Used (unattributed)".to_owned(),
                tokens: 20,
            },
            ContextCategoryView {
                label: "Provider-observed (est.)".to_owned(),
                tokens: 4_970,
            },
        ],
        ..ContextBreakdownView::default()
    };
    let categories = occupying_categories(&breakdown, 5_000);
    assert_eq!(
        categories
            .iter()
            .find(|(label, _)| label == "Used (unattributed)")
            .map(|(_, tokens)| *tokens),
        Some(20)
    );
    assert_eq!(
        categories
            .iter()
            .find(|(label, _)| label == "Provider-observed (est.)")
            .map(|(_, tokens)| *tokens),
        Some(4_970)
    );
    assert_eq!(categories.len(), 3, "{categories:?}");
}

#[test]
fn an_unavailable_projection_states_so_and_draws_no_grid() {
    let theme = Theme::dark();
    let usage = ContextUsage {
        breakdown: None,
        prepared_input: None,
        total_window: None,
        admitted_ceiling: None,
        gauge_percent: None,
        display_percent: None,
        ceiling_excess: None,
        evidence: "unavailable".to_owned(),
        status: "unavailable".to_owned(),
        non_guarantee: None,
    };
    let lines = text_of(&context_lines(&usage, &chrome(Some(85)), 120, &theme));
    assert!(lines.iter().any(|line| line.contains("unavailable")));
    assert!(!lines.iter().any(|line| line.contains(GRID_OCCUPIED)));
}

#[test]
fn a_live_in_turn_projection_is_current_not_stale() {
    let theme = Theme::dark();
    let live = usage(36_700, 1_000_000, Some(85));
    let lines = context_lines(&live, &chrome(Some(85)), 120, &theme);
    let text = text_of(&lines);
    assert!(!text.iter().any(|line| line.contains("Stale")), "{text:?}");
    assert!(!text.iter().any(|line| line.contains("unavailable")));
    assert!(
        !lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style == theme.error),
        "{text:?}"
    );
}

fn percent_meter(
    label: &str,
    used: u8,
    left: u8,
    resets: &str,
    group: Option<&str>,
) -> UsageMeterView {
    UsageMeterView {
        label: label.to_owned(),
        group: group.map(ToOwned::to_owned),
        reading: UsageMeterReading::Percent {
            floored_percent: used,
            exact_percent: format!("{used}.4"),
            percent_left: Some(left),
        },
        resets_at: Some(resets.to_owned()),
    }
}

fn limits_fixture() -> UsageLimitsView {
    UsageLimitsView {
        verticals: vec![
            VerticalUsageView {
                vertical: UsageVertical::new("codex", "Codex"),
                account_email: Some("operator@example.com".to_owned()),
                plan_label: Some("Pro".to_owned()),
                observed_at: None,
                state: VerticalUsageState::Ready {
                    meters: vec![
                        percent_meter("Weekly limit", 1, 99, "23:44 on 6 Sep", None),
                        percent_meter("5h limit", 78, 22, "16:00 on 3 Sep", None),
                        percent_meter(
                            "Weekly limit",
                            20,
                            80,
                            "23:44 on 6 Sep",
                            Some("GPT-5.3-Codex-Spark limit"),
                        ),
                    ],
                },
                balances: Vec::new(),
            },
            VerticalUsageView {
                vertical: UsageVertical::new("grok", "SuperGrok"),
                account_email: None,
                plan_label: None,
                observed_at: None,
                state: VerticalUsageState::NotConnected,
                balances: Vec::new(),
            },
            VerticalUsageView {
                vertical: UsageVertical::new("claude", "Claude"),
                account_email: None,
                plan_label: None,
                observed_at: None,
                state: VerticalUsageState::NotConnected,
                balances: Vec::new(),
            },
            VerticalUsageView {
                vertical: UsageVertical::new("openrouter", "OpenRouter"),
                account_email: None,
                plan_label: Some("API key · free tier".to_owned()),
                observed_at: None,
                state: VerticalUsageState::Ready {
                    meters: vec![UsageMeterView {
                        label: "Key limit".to_owned(),
                        group: None,
                        reading: UsageMeterReading::Amounts {
                            entries: vec![
                                UsageAmountView {
                                    label: "cap".to_owned(),
                                    value: "$10.00".to_owned(),
                                },
                                UsageAmountView {
                                    label: "remaining".to_owned(),
                                    value: "$6.42".to_owned(),
                                },
                            ],
                        },
                        resets_at: None,
                    }],
                },
                balances: Vec::new(),
            },
        ],
        session: SessionLedgerView {
            input_tokens: 1_234_567,
            cached_input_tokens: 1_000_000,
            output_tokens: 45_678,
            reasoning_tokens: 12_000,
            model_calls: 42,
            reporting_model_calls: 40,
            turn_wall_time: Some("3m12s".to_owned()),
            cost: UsageCostView::default(),
        },
        stale_age_seconds: None,
    }
}

/// A provider that named its period without a usage figure gets a row in
/// words with the reset it reported, and never a bar or a percentage.
#[test]
fn an_unreported_reading_renders_in_words_with_its_reset_and_no_bar() {
    let theme = Theme::dark();
    let mut limits = limits_fixture();
    limits.verticals[1].state = VerticalUsageState::Ready {
        meters: vec![UsageMeterView {
            label: "Weekly limit".to_owned(),
            group: None,
            reading: UsageMeterReading::Unreported,
            resets_at: Some("13:22 on 10 Sep".to_owned()),
        }],
    };
    limits.verticals[1].balances = vec![crate::tui::presentation::UsageBalanceView {
        label: "Pay as you go".to_owned(),
        value: "not enabled".to_owned(),
    }];
    let lines = text_of(&limits_lines(
        &limits,
        &LimitsChrome {
            refresh: None,
            refresh_failure_notes: &[],
            read_note: None,
            latest_turn: None,
        },
        120,
        &theme,
    ));
    let row = lines
        .iter()
        .find(|line| line.contains(crate::tui::presentation::USAGE_NOT_REPORTED))
        .expect("the unmetered period is a row");
    assert!(row.starts_with("Weekly limit"), "{row}");
    assert!(row.ends_with("(resets 13:22 on 10 Sep)"), "{row}");
    assert!(!row.contains(LIMIT_BAR_CELL) && !row.contains('%'), "{row}");
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("no allowance meters")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("Pay as you go") && line.ends_with("not enabled")),
        "{lines:?}"
    );
}

#[test]
fn all_four_verticals_are_listed_and_non_ready_ones_draw_no_bar() {
    let theme = Theme::dark();
    let lines = text_of(&limits_lines(
        &limits_fixture(),
        &LimitsChrome {
            refresh: None,
            refresh_failure_notes: &[],
            read_note: None,
            latest_turn: None,
        },
        120,
        &theme,
    ));
    for label in ["Codex", "SuperGrok", "Claude", "OpenRouter"] {
        assert!(
            lines.iter().any(|line| line.contains(label)),
            "{label} must be listed"
        );
    }
    // A non-Ready vertical is one plain line that names the CLI remedy.
    assert!(
        lines
            .iter()
            .any(|line| line == "not connected · run `plak login grok`"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "not connected · run `plak login claude`")
    );
    let claude = lines
        .iter()
        .position(|line| line == "Claude")
        .expect("the Claude block is listed");
    assert!(
        !lines[claude + 1].contains(LIMIT_BAR_CELL) && !lines[claude + 1].contains("0%"),
        "Claude is never drawn as a bar and never shown as 0%"
    );
}

/// Spec 02 §7.2: the vertical header is the vertical, the account line is its
/// own line, and a group renders as an indented sub-block.
#[test]
fn a_vertical_renders_its_account_line_bars_and_group_sub_block() {
    let theme = Theme::dark();
    let lines = text_of(&limits_lines(
        &limits_fixture(),
        &LimitsChrome {
            refresh: None,
            refresh_failure_notes: &[],
            read_note: None,
            latest_turn: None,
        },
        120,
        &theme,
    ));
    assert!(
        lines
            .iter()
            .any(|line| line == "Account: operator@example.com (Pro)"),
        "{lines:?}"
    );
    let bar_at = |line: &str| line.find('[');
    let weekly = lines
        .iter()
        .find(|line| line.contains("Weekly limit") && line.contains("99% left"))
        .expect("weekly");
    let five_h = lines
        .iter()
        .find(|line| line.contains("5h limit") && line.contains("22% left"))
        .expect("5h");
    let spark_weekly = lines
        .iter()
        .find(|line| line.contains("Weekly limit") && line.contains("80% left"))
        .expect("spark weekly");
    assert_eq!(bar_at(weekly), bar_at(five_h), "{weekly} vs {five_h}");
    assert_eq!(
        bar_at(weekly),
        bar_at(spark_weekly),
        "{weekly} vs {spark_weekly}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "GPT-5.3-Codex-Spark limit:")
    );
    // OpenRouter carries only a plan; the account line still states it.
    assert!(
        lines
            .iter()
            .any(|line| line == "Account: API key · free tier")
    );
}

/// One meter row, as plain text plus the style of each filled bar cell.
fn meter_row_line(percent_left: u8, theme: &Theme) -> ratatui::text::Line<'static> {
    let meter = UsageMeterView {
        label: "Weekly limit".to_owned(),
        group: None,
        reading: UsageMeterReading::Percent {
            floored_percent: 100 - percent_left,
            exact_percent: "0".to_owned(),
            percent_left: Some(percent_left),
        },
        resets_at: None,
    };
    let columns = super::LimitsColumns {
        label: meter.label.chars().count(),
        amount: 0,
    };
    super::meter_row(
        &meter,
        "",
        &columns,
        theme,
        theme.slash_item,
        theme.slash_item_help,
    )
}

fn meter_row_text(percent_left: u8, theme: &Theme) -> (String, Option<Style>) {
    let line = meter_row_line(percent_left, theme);
    // The fill span is always present, even at zero cells, so its style is
    // asserted at every percentage.
    let fill = line.spans.get(2).map(|span| span.style);
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    (text, fill)
}

/// The bar fill *is* the percent left: full bar = nothing used, empty bar =
/// limit reached (spec 02 §7.2).
#[test]
fn the_bar_fill_is_the_percent_left_over_twenty_cells() {
    let theme = Theme::dark();
    for percent in 0_u8..=100 {
        let (text, _) = meter_row_text(percent, &theme);
        let cells = text.chars().filter(|ch| *ch == LIMIT_BAR_CELL).count();
        assert_eq!(cells, LIMIT_BAR_CELLS, "percent {percent}");
    }
    let filled_cells = |percent: u8| -> usize {
        usize::from(percent)
            .saturating_mul(LIMIT_BAR_CELLS)
            .saturating_div(100)
    };
    for percent in [0_u8, 1, 99, 100] {
        let (text, _) = meter_row_text(percent, &theme);
        let inside = text
            .split_once('[')
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(bar, _)| bar)
            .expect("the bar is bracketed");
        let filled = filled_cells(percent);
        assert_eq!(inside.chars().count(), LIMIT_BAR_CELLS);
        assert!(inside.chars().all(|ch| ch == LIMIT_BAR_CELL));
        let line = meter_row_line(percent, &theme);
        assert_eq!(line.spans[2].content.chars().count(), filled);
        assert!(line.spans[2].content.chars().all(|ch| ch == LIMIT_BAR_CELL));
        assert_eq!(
            line.spans[3].content.chars().count(),
            LIMIT_BAR_CELLS - filled
        );
        assert!(line.spans[3].content.chars().all(|ch| ch == LIMIT_BAR_CELL));
    }
    let bar = |percent| meter_row_text(percent, &theme).0;
    // The number is right-aligned in its own column, so a two- and a
    // three-digit percentage keep the same column.
    assert!(bar(99).contains("]  99% left"), "{}", bar(99));
    assert!(bar(100).contains("] 100% left"), "{}", bar(100));
}

/// Colour states the allowance that remains, from theme tokens only.
#[test]
fn the_bar_fill_colour_tracks_the_allowance_that_remains() {
    let theme = Theme::dark();
    for (percent, expected) in [
        (100_u8, theme.success),
        (50, theme.success),
        (49, theme.warning),
        (20, theme.warning),
        (19, theme.error),
        (1, theme.error),
    ] {
        assert_eq!(
            meter_row_text(percent, &theme).1,
            Some(expected),
            "percent {percent}"
        );
    }
}

/// An exhausted allowance is the one state that must stand out.
///
/// At 0% left there are no fill cells, so colouring only the fill left the
/// row in ordinary body colour — visually identical to a healthy neutral
/// row. The empty track carries the colour instead, and the percentage
/// always states its own health.
#[test]
fn an_exhausted_limit_is_coloured_rather_than_left_neutral() {
    let theme = Theme::dark();
    let styles = |percent: u8| {
        let meter = UsageMeterView {
            label: "Weekly limit".to_owned(),
            group: None,
            reading: UsageMeterReading::Percent {
                floored_percent: 100 - percent,
                exact_percent: "0".to_owned(),
                percent_left: Some(percent),
            },
            resets_at: None,
        };
        let columns = super::LimitsColumns {
            label: meter.label.chars().count(),
            amount: 0,
        };
        let line = super::meter_row(
            &meter,
            "",
            &columns,
            &theme,
            theme.slash_item,
            theme.slash_item_help,
        );
        // spans: label, "[", fill, track, "]", " NNN%", " left"
        (
            line.spans[3].style,
            line.spans[5].style,
            line.spans[3].content.to_string(),
        )
    };

    let (track, number, cells) = styles(0);
    assert_eq!(
        cells.chars().count(),
        LIMIT_BAR_CELLS,
        "the bar is all track"
    );
    assert_eq!(track, theme.error, "an empty bar is coloured, not muted");
    assert_eq!(number, theme.error, "and so is `0%`");

    // A healthy bar keeps its darker stippled track; only the fill and
    // the number carry the state.
    let (track, number, _) = styles(99);
    assert_eq!(track, theme.bar_track);
    assert_eq!(number, theme.success);

    let (_, number, _) = styles(30);
    assert_eq!(number, theme.warning);
}

#[test]
fn percent_left_falls_back_to_the_complement_of_the_used_percent() {
    assert_eq!(percent_left_of(0, None), 100);
    assert_eq!(percent_left_of(22, None), 78);
    assert_eq!(percent_left_of(100, None), 0);
    assert_eq!(percent_left_of(22, Some(77)), 77);
    assert_eq!(percent_left_of(0, Some(200)), 100);
}

#[test]
fn an_unknown_cost_is_never_printed_as_zero_dollars() {
    let lines = session_ledger_lines(&limits_fixture());
    let cost = lines
        .iter()
        .find(|line| line.contains("Cost:"))
        .expect("the ledger reports a cost row");
    assert!(cost.contains("not available"));
    assert!(!cost.contains("$0"));
    assert!(lines[0].contains("1,234,567 (1,000,000 cached)"));
}

#[test]
fn a_stale_limits_projection_states_its_age() {
    let theme = Theme::dark();
    let mut stale = limits_fixture();
    stale.stale_age_seconds = Some(95);
    let lines = text_of(&limits_lines(
        &stale,
        &LimitsChrome {
            refresh: None,
            refresh_failure_notes: &[],
            read_note: None,
            latest_turn: None,
        },
        120,
        &theme,
    ));
    assert!(
        lines
            .iter()
            .any(|line| line == "Stale · this projection is 1m35s old")
    );
}

fn session_fixture() -> SessionInfoView {
    SessionInfoView {
        host_version: "0.1.0".to_owned(),
        session_id: "sid-123abc".to_owned(),
        working_dir: "/home/user/project".to_owned(),
        root_template: None,
        root_model: Some("gpt-5.6-luna".to_owned()),
        root_effort: Some("medium".to_owned()),
        thinking: "reasoning summaries (auto) requested at medium".to_owned(),
        tool_policy: Some("Code Mode (native-ptc)".to_owned()),
        admitted_options: vec!["extra = priority".to_owned()],
        api_kernel: Some("Responses".to_owned()),
        turn_number: Some(5),
        tree_epoch: None,
        context: Some(crate::tui::presentation::SessionContextView {
            reported_window: Some(1_000_000),
            admitted_input: Some(1_000_000),
            evidence: "reference-client-envelope".to_owned(),
        }),
    }
}

fn live() -> SessionLive<'static> {
    SessionLive {
        context: Some("36.7k / 1.0m tokens (3.7%)".to_owned()),
        queue: "0 queued · live auto-drain".to_owned(),
        tree_epoch: Some("live"),
        build: "plak-test-build".to_owned(),
    }
}

#[test]
fn the_session_tab_names_the_running_build() {
    let rows = session_rows(&session_fixture(), &live());
    assert_eq!(rows[1].label, "Build");
    assert_eq!(rows[1].value, "plak-test-build");
}

#[test]
fn session_rows_state_absent_fields_and_copy_as_label_value_lines() {
    let rows = session_rows(&session_fixture(), &live());
    let copy = session_copy_text(&rows);
    assert!(copy.contains("Session ID: sid-123abc"));
    assert!(copy.contains("Working directory: /home/user/project"));
    assert!(copy.contains("Tool policy: Code Mode (native-ptc)"));
    assert!(copy.contains("API kernel: Responses"));
    assert!(copy.contains("Context: 36.7k / 1.0m tokens (3.7%)"));
    assert!(copy.contains("Tree epoch: live"));
    assert!(
        !copy.contains("Binding:"),
        "root model/effort are the root-agent items; no duplicate Binding row\n{copy}"
    );
    assert_eq!(copy.lines().count(), rows.len());
}

/// Before the live gauge lands, the host's admitted envelope states the
/// window; the host's tree epoch wins over the chrome's snapshot label.
#[test]
fn session_rows_fall_back_to_the_host_envelope_and_epoch() {
    let mut info = session_fixture();
    info.tree_epoch = Some("tree_terminal".to_owned());
    let mut live = live();
    live.context = None;
    let copy = session_copy_text(&session_rows(&info, &live));
    assert!(
        copy.contains("Context: 1.0m tokens admitted · window 1.0m · reference-client-envelope"),
        "{copy}"
    );
    assert!(copy.contains("Tree epoch: tree_terminal"), "{copy}");
    info.context = None;
    let copy = session_copy_text(&session_rows(&info, &live));
    assert!(copy.contains("Context: not reported"), "{copy}");
}

/// The operator's complaint: `Binding` repeated the model and effort rows,
/// and `Root template` repeated the model. Neither may come back.
#[test]
fn the_session_cluster_carries_no_duplicate_rows() {
    let mut info = session_fixture();
    // A template equal to the model is the model line; it earns no row.
    info.root_template.clone_from(&info.root_model);
    let rows = session_rows(&info, &live());
    let labels: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
    assert!(!labels.contains(&"Binding"), "{labels:?}");
    assert!(!labels.contains(&"Root template"), "{labels:?}");
    assert!(!labels.contains(&"Template"), "{labels:?}");
    for (index, label) in labels.iter().enumerate() {
        assert!(
            !labels[..index].contains(label),
            "{label} is listed twice: {labels:?}"
        );
    }
    let duplicates = rows
        .iter()
        .filter(|row| row.value == "gpt-5.6-luna")
        .count();
    assert_eq!(duplicates, 1, "the model is stated exactly once: {rows:?}");

    // A template that differs from the model earns exactly one row.
    info.root_template = Some("reviewer".to_owned());
    let rows = session_rows(&info, &live());
    let templates: Vec<&SessionRow> = rows.iter().filter(|row| row.label == "Template").collect();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].value, "reviewer");
}

#[test]
fn compact_rows_cluster_and_every_row_has_a_click_target() {
    let theme = Theme::dark();
    let rows = session_rows(&session_fixture(), &live());
    let (lines, hits) = session_lines(&rows, &theme);
    assert_eq!(hits.len(), rows.len(), "every row is clickable");
    let rendered = text_of(&lines);
    for (index, line) in &hits {
        assert!(rendered[*line].contains(&rows[*index].value));
    }
    // The runtime cluster has no blank rows inside it.
    let model = hits
        .iter()
        .find(|(index, _)| rows[*index].label == "Model")
        .expect("Model row")
        .1;
    let effort = hits
        .iter()
        .find(|(index, _)| rows[*index].label == "Effort")
        .expect("Effort row")
        .1;
    assert_eq!(effort, model + 1, "consecutive compact rows are adjacent");
}
