#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use plakous_host_protocol::views::SourceLink;
use plakous_host_protocol::views::transcript::AutoContinue;

#[test]
fn a_compact_result_indents_like_serde_pretty_and_keeps_key_order() {
    for json in [
        r#"{"a":1,"b":[true,null,{"c":"x"}],"d":{},"e":[]}"#,
        r#"[{"ids":[1, 2],"severity":"low"}]"#,
        r#"{"s":"a, \"quoted\" {brace} [x]: y\\"}"#,
    ] {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(
            indent_json(json),
            serde_json::to_string_pretty(&value).unwrap(),
            "{json}"
        );
    }
    assert_eq!(
        indent_json(r#"{"verdict":"ok","findings":[]}"#),
        "{\n  \"verdict\": \"ok\",\n  \"findings\": []\n}"
    );
    // A cut result keeps its cut line; prose and indented text stay.
    assert_eq!(
        indent_result("{\"notes\":[\"ab\",\"a\n… cut at 16 KiB of 40 KiB"),
        "{\n  \"notes\": [\n    \"ab\",\n    \"a\n… cut at 16 KiB of 40 KiB"
    );
    for text in ["All goldens pass.", "{\n  \"ok\": true\n}"] {
        assert!(matches!(indent_result(text), Cow::Borrowed(same) if same == text));
    }
}

#[test]
fn a_process_row_names_long_runs_in_minutes_and_hours() {
    let row = |ms| {
        ScrollBlock::from_row(TranscriptRow::Process(ProcessNotice {
            job_id: "p261".into(),
            outcome: ProcessOutcome::Exited { code: Some(0) },
            duration_ms: Some(ms),
            subject: None,
        }))
    };
    for (ms, expected) in [
        (1_300_000, "job p261 exited 0 · 21m40s"),
        (3_900_000, "job p261 exited 0 · 1h05m"),
        (12_300, "job p261 exited 0 · 12.3s"),
    ] {
        assert_eq!(row(ms), Some(ScrollBlock::item(expected)));
    }
}

#[test]
fn a_process_row_with_its_command_reads_as_the_command() {
    let row = |outcome, duration_ms| {
        ScrollBlock::from_row(TranscriptRow::Process(ProcessNotice {
            job_id: "p261".into(),
            outcome,
            duration_ms,
            subject: Some("just check".into()),
        }))
    };
    assert_eq!(
        row(ProcessOutcome::Exited { code: Some(0) }, Some(1_300_000)),
        Some(ScrollBlock::Item {
            text: "p261 just check".into(),
            meta: vec!["exited 0".into(), "21m40s".into()],
        })
    );
    assert_eq!(
        row(ProcessOutcome::Stopped, None),
        Some(ScrollBlock::Item {
            text: "p261 just check".into(),
            meta: vec!["stopped".into()],
        })
    );
}

/// 2026-10-07 14:32:00 UTC.
const SENT_MS: u64 = 1_791_383_520_000;

#[test]
fn a_send_time_reads_as_the_local_clock_today_and_with_its_date_otherwise() {
    let today = PromptClock::at(SENT_MS + 3_600_000, 0);
    assert_eq!(today.label(SENT_MS).as_deref(), Some("14:32"));
    let tomorrow = PromptClock::at(SENT_MS + 86_400_000, 0);
    assert_eq!(tomorrow.label(SENT_MS).as_deref(), Some("Oct 7 14:32"));
    // Local time: two hours east it is 16:32, still the same day.
    let east = PromptClock::at(SENT_MS, 7_200);
    assert_eq!(east.label(SENT_MS).as_deref(), Some("16:32"));
    // Ten hours east the send time falls on the next local day.
    let far_east = PromptClock::at(SENT_MS, 36_000);
    assert_eq!(far_east.label(SENT_MS).as_deref(), Some("00:32"));
    assert_eq!(
        PromptClock::default().label(SENT_MS).as_deref(),
        Some("Oct 7 14:32")
    );
}

/// A fixed startup offset would show a winter stamp at the summer hour.
/// The zone's offset is read at the stamp.
#[test]
fn a_zone_offset_follows_the_stamp_across_a_daylight_saving_change() {
    // New York's rule as a POSIX TZ string: no host tzdata involved.
    let zone = jiff::tz::TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").expect("posix tz");
    // jiff's own New York examples: 2024-01-10 21:46:44 EST, 2024-07-08 22:46:44 EDT.
    let winter_ms = 1_704_941_204_000;
    let summer_ms = 1_720_493_204_000;
    assert_eq!(offset_at(&zone, winter_ms), -5 * 3_600);
    assert_eq!(offset_at(&zone, summer_ms), -4 * 3_600);
    let winter = local_moment(winter_ms, offset_at(&zone, winter_ms)).unwrap();
    let summer = local_moment(summer_ms, offset_at(&zone, summer_ms)).unwrap();
    assert_eq!((winter.hour(), winter.minute()), (21, 46));
    assert_eq!((summer.hour(), summer.minute()), (22, 46));
    let shifted = local_moment(winter_ms, -4 * 3_600).unwrap();
    assert_eq!(shifted.hour(), 22);
}

#[test]
fn a_zone_labels_each_stamp_at_its_own_offset() {
    static NEW_YORK: std::sync::LazyLock<jiff::tz::TimeZone> = std::sync::LazyLock::new(|| {
        jiff::tz::TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").expect("posix tz")
    });
    // Summer now, a winter stamp: the stamp keeps its own (EST) offset.
    let clock = PromptClock::in_zone(&NEW_YORK, 1_720_493_204_000);
    assert_eq!(
        clock.label(1_704_941_204_000).as_deref(),
        Some("Jan 10 21:46")
    );
    assert_eq!(clock.label(1_720_493_204_000).as_deref(), Some("22:46"));
}

#[test]
fn a_prompt_send_time_is_right_aligned_and_dropped_first_at_narrow_widths() {
    let theme = super::super::theme::Theme::dark();
    let prefs = RenderPrefs {
        prompt_clock: PromptClock::at(SENT_MS, 0),
        ..RenderPrefs::default()
    };
    let first_row = |block: &ScrollBlock, width: usize| {
        let plan = plan_rows(std::slice::from_ref(block), prefs);
        render::block_lines(block, &plan[0], &theme, width, prefs)[0].to_string()
    };
    let user = ScrollBlock::User {
        text: "ship it".into(),
        sent_at_ms: Some(SENT_MS),
    };
    let steer = ScrollBlock::Steer {
        text: "also docs".into(),
        sent_at_ms: Some(SENT_MS),
    };
    let wide = first_row(&user, 40);
    assert!(wide.trim_end().ends_with(" 14:32"), "{wide:?}");
    assert_eq!(wide.chars().count(), 40);
    let wide = first_row(&steer, 40);
    assert!(wide.trim_end().ends_with("steer 14:32"), "{wide:?}");
    // Too narrow for the time and twenty text columns: the time goes,
    // the steer tag stays.
    let narrow = first_row(&user, 26);
    assert!(!narrow.contains("14:32"), "{narrow:?}");
    assert_eq!(
        narrow.trim_end(),
        first_row(&ScrollBlock::user("ship it".into()), 26).trim_end()
    );
    let narrow = first_row(&steer, 30);
    assert!(!narrow.contains("14:32"), "{narrow:?}");
    assert!(narrow.trim_end().ends_with("steer"), "{narrow:?}");
    // A long prompt wraps short of the time on its first row only.
    let long = ScrollBlock::User {
        text: "word ".repeat(30).trim_end().to_owned(),
        sent_at_ms: Some(SENT_MS),
    };
    let plan = plan_rows(std::slice::from_ref(&long), prefs);
    let rows = render::block_lines(&long, &plan[0], &theme, 60, prefs);
    assert!(rows[0].to_string().trim_end().ends_with(" 14:32"));
    assert!(
        rows.iter()
            .skip(1)
            .all(|row| !row.to_string().contains("14:32"))
    );
    assert!(rows.iter().all(|row| row.width() <= 60));
}

#[test]
fn a_prompt_copy_leaves_out_the_send_time_and_the_steer_tag() {
    use crate::tui::theme::Theme;
    let theme = Theme::dark();
    let prefs = RenderPrefs {
        prompt_clock: PromptClock::at(SENT_MS, 0),
        ..RenderPrefs::default()
    };
    let blocks = vec![
        ScrollBlock::User {
            text: "ship it\n  keep indent".into(),
            sent_at_ms: Some(SENT_MS),
        },
        ScrollBlock::Steer {
            text: "also docs".into(),
            sent_at_ms: Some(SENT_MS),
        },
    ];
    let mut cache = TranscriptRenderCache::default();
    let render = cache.get(1, &blocks, &theme, 60, TranscriptChange::Structural, prefs);
    let painted = render
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert!(
        painted.iter().any(|line| line.contains("14:32")),
        "{painted:?}"
    );
    assert!(
        painted.iter().any(|line| line.contains("steer")),
        "{painted:?}"
    );
    let copied = render.layout.plain.join("\n");
    assert!(!copied.contains("14:32"), "{copied}");
    assert!(!copied.contains("steer"), "{copied}");
    assert!(copied.contains("ship it"), "{copied}");
    assert!(
        copied.lines().any(|line| line.contains("  keep indent")),
        "{copied}"
    );
    assert!(copied.contains("also docs"), "{copied}");
}

#[test]
fn a_tool_row_is_labelled_by_its_exposed_name() {
    let row = |exposed: Option<&str>| {
        ScrollBlock::from_row(TranscriptRow::Tool(ToolItem {
            call_id: "c1".into(),
            name: "apply_patch".into(),
            exposed_name: exposed.map(str::to_owned),
            kind: ActivityKind::Tool,
            state: "running".into(),
            subject: None,
            detail: None,
            started_at_ms: None,
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        }))
    };
    assert!(matches!(row(Some("edit")), Some(ScrollBlock::Tool { name, .. }) if name == "edit"));
    assert!(matches!(
        row(None),
        Some(ScrollBlock::Tool { name, .. }) if name == "apply_patch"
    ));
}

#[test]
fn from_row_maps_every_row_type() {
    use plakous_host_protocol::views::transcript::{
        AgentMilestone, MilestoneEvent, NoticeScope, ProcessNotice, ProcessOutcome, RetryItem,
        ThinkingRow,
    };
    let block = |row| ScrollBlock::from_row(row);
    assert_eq!(
        block(TranscriptRow::user("hi")),
        Some(ScrollBlock::user("hi".into()))
    );
    assert_eq!(
        block(TranscriptRow::steer("status update?")),
        Some(ScrollBlock::steer("status update?".into()))
    );
    assert_eq!(
        block(TranscriptRow::User {
            text: "hi".into(),
            sent_at_ms: Some(7),
        }),
        Some(ScrollBlock::User {
            text: "hi".into(),
            sent_at_ms: Some(7),
        })
    );
    assert_eq!(
        block(TranscriptRow::assistant("ok")),
        Some(ScrollBlock::Assistant("ok".into()))
    );
    assert!(matches!(
        block(TranscriptRow::Thinking(ThinkingRow {
            location_key: "0:0".into(),
            text: "inventory the repo".into(),
            elapsed_ms: Some(4_100),
        })),
        Some(ScrollBlock::Thinking {
            location_key,
            text,
            sealed: true,
            elapsed_ms: Some(4_100),
            ..
        }) if location_key == "0:0" && text == "inventory the repo"
    ));
    assert_eq!(
        block(TranscriptRow::error(NoticeScope::Turn, "boom")),
        Some(ScrollBlock::Error("boom".into()))
    );
    assert_eq!(
        block(TranscriptRow::info(NoticeScope::Queue, "queued")),
        Some(ScrollBlock::System("queued".into()))
    );
    assert_eq!(
        block(TranscriptRow::warning(
            NoticeScope::Clipped,
            "observation clipped"
        )),
        Some(ScrollBlock::item("observation clipped"))
    );
    assert_eq!(
        block(TranscriptRow::info(NoticeScope::Diagnostic, "internal")),
        None
    );
    assert_eq!(
        block(TranscriptRow::Interrupted),
        Some(ScrollBlock::System("⊘ interrupted".into()))
    );
    assert_eq!(
        block(TranscriptRow::Truncated),
        Some(ScrollBlock::System(TRANSCRIPT_TRUNCATION_MARKER.into()))
    );
    assert_eq!(
        block(TranscriptRow::Retry(RetryItem {
            attempt: 2,
            max_attempts: 4,
            cause: "rate limited".into(),
        })),
        Some(ScrollBlock::item("retry 2/4 · rate limited"))
    );
    assert_eq!(
        block(TranscriptRow::Process(ProcessNotice {
            job_id: "p3".into(),
            outcome: ProcessOutcome::Exited { code: Some(0) },
            duration_ms: None,
            subject: None,
        })),
        Some(ScrollBlock::item("job p3 exited 0"))
    );
    let spawned = block(TranscriptRow::Milestone(AgentMilestone {
        agent_id: "c23".into(),
        event: MilestoneEvent::Spawned,
        template: Some("worker".into()),
        text: Some("port the tests\nmore".into()),
        duration_ms: None,
        candidate: None,
        files: None,
        paths: Vec::new(),
    }))
    .unwrap();
    assert!(matches!(
        spawned,
        ScrollBlock::Milestone { open: false, .. }
    ));
    assert_eq!(
        spawned.as_plain_line(),
        "◆ spawned c23 · worker · port the tests"
    );
    let retasked = block(TranscriptRow::Milestone(AgentMilestone {
        agent_id: "c23".into(),
        event: MilestoneEvent::Retasked,
        template: None,
        text: Some("again".into()),
        duration_ms: None,
        candidate: None,
        files: None,
        paths: Vec::new(),
    }))
    .unwrap();
    // The hollow cousin of spawned `◆`: `↻` came from a fallback font
    // and rendered out of line.
    assert_eq!(retasked.as_plain_line(), "◇ re-tasked c23 · again");
    // A stored auto-continue row (sessions from before the host stopped
    // writing them) renders nothing, restored or live.
    assert_eq!(
        block(TranscriptRow::AutoContinue(AutoContinue {
            agents_finished: 1,
            processes_exited: 2,
            agents: vec!["c1".into()],
        })),
        None
    );
    assert!(matches!(
        block(TranscriptRow::SessionHeader {
            session_id: "0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000".into(),
            carried_from: None,
        }),
        Some(ScrollBlock::Header(text)) if text.starts_with("session ")
    ));
}

#[test]
fn plain_roundtrip_labels() {
    let u = ScrollBlock::user("x".into());
    assert_eq!(u.as_plain_line(), "you: x");
    assert_eq!(u.body(), "x");
}

#[test]
fn sticky_when_scrolled_past_user() {
    let blocks = vec![
        ScrollBlock::user("hello world".into()),
        ScrollBlock::Assistant("hi there friend".into()),
        ScrollBlock::user("second".into()),
        ScrollBlock::Assistant("ok".into()),
    ];
    let layout = layout_blocks(&blocks, 40);
    // Scroll so first user is above viewport
    let sticky = sticky_user_lines(&blocks, &layout, 3, 2, 40);
    assert!(sticky.is_some());
    let s = sticky.unwrap();
    assert!(
        s.lines[0].starts_with(PROMPT_PREFIX)
            || s.lines[0].starts_with('┃')
            || s.lines[0].starts_with("> "),
        "sticky first line: {:?}",
        s.lines[0]
    );
    assert!(
        s.lines[0].contains("hello")
            || s.lines
                .iter()
                .any(|l| l.contains("second") || l.contains("hello"))
    );
}

#[test]
fn tool_elapsed_labels_match_done_state() {
    assert_eq!(format_tool_elapsed(Duration::from_millis(12)), "12ms");
    assert_eq!(format_tool_elapsed(Duration::from_millis(1200)), "1.2s");
    assert_eq!(tool_duration_label("done:1.2s"), Some("1.2s"));
    assert_eq!(tool_duration_label("running"), None);
    assert_eq!(tool_state_detail("done:1.2s"), None);
}

/// The estimator promises to agree with the measured
/// layout on "whether a block draws any rows at all". An empty or
/// whitespace assistant block — an empty first delta makes one — measured
/// zero rows and estimated one, so a block the anchor landed on vanished
/// when it was promoted and the viewport fell to the tail.
#[test]
fn an_empty_assistant_block_estimates_the_nothing_it_draws() {
    let width = 80;
    let theme = crate::tui::Theme::dark();
    for text in ["", "   ", "\n\n"] {
        let block = ScrollBlock::Assistant(text.to_owned());
        let drew = render::block_lines(
            &block,
            &RowPlan::default(),
            &theme,
            width,
            RenderPrefs::default(),
        )
        .len();
        let estimated = render::estimate_rows_for_test(&block, width, RenderPrefs::default());
        assert_eq!(
            (drew == 0, estimated == 0),
            (true, true),
            "empty assistant text: drew {drew} rows, estimated {estimated}"
        );
    }
    // A block with content still estimates at least one row.
    assert!(
        render::estimate_rows_for_test(
            &ScrollBlock::Assistant("hello".into()),
            width,
            RenderPrefs::default(),
        ) > 0
    );
}

#[test]
fn a_bare_terminal_update_never_clears_a_recorded_duration() {
    // The yielded Compose head: `done:12.3s` measured at the exec terminal,
    // then a later `emit_compose_update` re-sends a bare `done`.
    assert_eq!(
        terminal_state_keeping_duration("done:12.3s", "done".to_owned()),
        "done:12.3s"
    );
    assert_eq!(
        terminal_state_keeping_duration("failed · 1.2s", "failed".to_owned()),
        "failed · 1.2s"
    );
    // A later terminal that changes the verdict keeps the measured elapsed.
    assert_eq!(
        terminal_state_keeping_duration("done:12.3s", "failed".to_owned()),
        "failed · 12.3s"
    );
    // An update that carries its own timing wins outright.
    assert_eq!(
        terminal_state_keeping_duration("done:12.3s", "done:14.1s".to_owned()),
        "done:14.1s"
    );
    // Nothing to preserve, and no invented duration.
    assert_eq!(
        terminal_state_keeping_duration("running", "done".to_owned()),
        "done"
    );
    // A rejection reason that merely contains a separator is not a duration.
    assert_eq!(recorded_tool_duration("rejected: a · b"), None);
    assert_eq!(
        recorded_tool_duration("rejected: denied · 340ms"),
        Some("340ms")
    );
}

#[test]
fn sticky_uses_gb_prompt_glyph() {
    let blocks = vec![
        ScrollBlock::user("hello".into()),
        ScrollBlock::Assistant("ok".into()),
    ];
    let layout = layout_blocks(&blocks, 40);
    let sticky = sticky_user_lines(&blocks, &layout, 2, 2, 40).unwrap();
    assert!(
        sticky.lines[0].starts_with(PROMPT_PREFIX),
        "got {:?}",
        sticky.lines[0]
    );
    assert!(sticky.lines[0].contains(PROMPT_GLYPH));
}

#[test]
fn sticky_waits_until_the_complete_user_block_is_above_viewport() {
    let blocks = vec![
        ScrollBlock::user("one two three four five six seven eight".into()),
        ScrollBlock::Assistant("answer".into()),
    ];
    let layout = layout_blocks(&blocks, 12);
    let user_rows = layout
        .block_index
        .iter()
        .take_while(|row| **row == Some(0))
        .count();
    assert!(user_rows > 1);
    assert!(sticky_user_lines(&blocks, &layout, 1, 3, 12).is_none());
    assert!(sticky_user_lines(&blocks, &layout, user_rows, 3, 12).is_some());
}

#[test]
fn sticky_reverse_scan_finds_user_at_start_when_scrolled_deep() {
    let mut blocks = vec![ScrollBlock::user("early prompt".into())];
    for i in 0..200 {
        blocks.push(ScrollBlock::Assistant(format!("assistant filler {i}")));
    }
    let layout = layout_blocks(&blocks, 40);
    let visible_start = layout.plain.len().saturating_sub(5);
    let sticky = sticky_user_lines(&blocks, &layout, visible_start, 3, 40);
    assert!(sticky.is_some());
    let s = sticky.unwrap();
    assert!(s.lines.iter().any(|l| l.contains("early prompt")));
    assert_eq!(s.source_start, 0);
}

fn tool(name: &str, state: &str, subject: &str) -> ScrollBlock {
    ScrollBlock::tool_row(
        Some(format!("{name}-{state}-{subject}")),
        name,
        state,
        ActivityKind::Tool,
        Some(subject.to_owned()),
        None,
    )
}

/// The plain projection at 100 cells, with an item row's right-aligned
/// padding collapsed to two spaces: `  ✓ $ ls  2.4s ▸`.
fn plain(blocks: &[ScrollBlock], prefs: RenderPrefs) -> Vec<String> {
    layout_blocks_with(blocks, 100, prefs)
        .plain
        .into_iter()
        .map(|row| collapse_padding(&row))
        .collect()
}

fn collapse_padding(row: &str) -> String {
    let body = row.trim_start();
    let indent = &row[..row.len() - body.len()];
    let mut out = indent.to_owned();
    let mut spaces = 0;
    for ch in body.chars() {
        if ch == ' ' {
            spaces += 1;
            continue;
        }
        out.push_str(if spaces >= 4 { "  " } else { &"    "[..spaces] });
        spaces = 0;
        out.push(ch);
    }
    out
}

#[test]
fn every_normative_header_format_renders_verbatim() {
    // Spec 02 §5.4 header table. The host supplies each subject; the renderer
    // prefixes the state glyph and the verb (Compose still has a fold).
    let rows = [
        (
            tool("exec_command", "done:2.4s", "cargo test"),
            "  ✓ $ cargo test  2.4s ▸",
        ),
        (
            tool("read_file", "done", "src/lib.rs (1-120 of 11k)"),
            "  ✓ Read src/lib.rs (1-120 of 11k)  ▸",
        ),
        (
            tool(
                "search_repository",
                "done",
                "\"ToolLine\" in crates (7 matches in 3 files)",
            ),
            "  ✓ Searched \"ToolLine\" in crates (7 matches in 3 files)  ▸",
        ),
        (
            tool("apply_patch", "done", "crates/plak/tui/src/ui.rs +12/-3"),
            "  ✓ Edited crates/plak/tui/src/ui.rs +12 −3  ▸",
        ),
        (
            tool("write_file", "done", "crates/plak/new.rs +4/-0"),
            "  ✓ Wrote crates/plak/new.rs  ▸",
        ),
        (
            tool("web_fetch", "done", "https://example.test/spec"),
            "  ✓ Fetched https://example.test/spec  ▸",
        ),
        (
            tool("list_dir", "done", "crates/plak (9 entries)"),
            "  ✓ Listed crates/plak (9 entries)  ▸",
        ),
        (
            tool("send_agent_message", "done", "c23 · please rebase"),
            "  ✓ Messaged c23 · please rebase  ▸",
        ),
        (
            tool("update_plan", "done", "3/9 done"),
            "  ✓ Updated plan · 3/9 done  ▸",
        ),
        (
            tool("weather_lookup", "done", "Zurich"),
            "  ✓ Weather lookup Zurich  ▸",
        ),
    ];
    for (block, expected) in rows {
        assert_eq!(plain(&[block], RenderPrefs::default())[0], expected);
    }
}

#[test]
fn a_command_that_ran_and_failed_paints_a_cross_and_how_it_ended() {
    assert_eq!(tool_state_glyph("failed:exit 1"), "×");
    assert_eq!(tool_state_detail("failed:exit 1"), Some("exit 1"));
    assert_eq!(tool_state_glyph("failed:timed out"), "×");
    assert_eq!(tool_state_detail("failed:timed out"), Some("timed out"));
    assert_eq!(
        terminal_state_keeping_duration("done:1.2s", "failed:exit 1".to_owned()),
        "failed:exit 1 · 1.2s"
    );
}

#[test]
fn the_state_glyph_carries_the_lifecycle_not_an_animation() {
    assert_eq!(tool_state_glyph("running"), "◌");
    assert_eq!(tool_state_glyph("done"), "✓");
    assert_eq!(tool_state_glyph("failed"), "×");
    assert_eq!(tool_state_glyph("rejected:OccupancyFence"), "⊘");
    assert_eq!(tool_state_glyph("cancelled"), "⊘");
}

#[test]
fn tool_payloads_render_only_in_an_item_the_operator_opened() {
    for name in [
        "exec_command",
        "read_file",
        "grep",
        "apply_patch",
        "custom_tool",
        "update_plan",
    ] {
        for mode in [DisplayMode::Collapsed, DisplayMode::Expanded] {
            let mut block = tool(name, "done", "subject");
            if let ScrollBlock::Tool {
                detail, mode: slot, ..
            } = &mut block
            {
                *detail = Some("raw result with ```json and Arguments: private".into());
                *slot = mode;
            }
            let rows = plain(&[block], RenderPrefs::default());
            let opened = mode == DisplayMode::Expanded;
            assert_eq!(
                rows.iter().any(|row| row.contains("private")),
                opened,
                "{name} {mode:?}: {rows:?}"
            );
            assert_eq!(rows.len() == 1, !opened, "{name} {mode:?}: {rows:?}");
        }
    }
}

#[test]
fn activity_status_uses_state_tokens_without_coloring_action_text() {
    for theme in [crate::tui::Theme::dark(), crate::tui::Theme::light()] {
        for (state, glyph, status) in [
            ("done", "✓", theme.success),
            ("running", "⠼", theme.busy),
            ("failed:exit 7", "✗", theme.error_label),
        ] {
            let block = tool("apply_patch", state, "a.rs +12/-3");
            let lines = render::block_lines(
                &block,
                &RowPlan::default(),
                &theme,
                80,
                RenderPrefs::default(),
            );
            assert!(
                lines[0]
                    .spans
                    .iter()
                    .any(|span| { span.content == glyph && span.style.fg == status.fg })
            );
            assert!(lines[0].spans.iter().any(|span| {
                span.content.contains("Edited a.rs") && span.style.fg == theme.tool.fg
            }));
        }
        let blocks = vec![
            tool("apply_patch", "done", "a.rs +12/-3"),
            ScrollBlock::Assistant("Done.".into()),
        ];
        let plan = plan_rows(&blocks, RenderPrefs::default());
        let lines = render::block_lines(&blocks[0], &plan[0], &theme, 80, RenderPrefs::default());
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| { span.content == "✓" && span.style.fg == theme.success.fg })
        );
    }
}

#[test]
fn activity_groups_keep_interruption_markers_and_unsuccessful_calls_visible() {
    for state in [
        "interrupted",
        "cancelled",
        "canceled",
        "cancelled · 1.2s",
        "cancelled after 1 of 2 · 1.2s",
    ] {
        for name in ["custom_tool", "read_file", "spawn_agent", "exec_js"] {
            for closed in [false, true] {
                for expanded in [false, true] {
                    let mut head = tool(name, state, "cancelled action");
                    if let ScrollBlock::Tool { group_open, .. } = &mut head {
                        *group_open = expanded;
                    }
                    let mut blocks = vec![head, ScrollBlock::System(INTERRUPTED_MARKER.into())];
                    if closed {
                        blocks.push(ScrollBlock::Assistant("Next answer.".into()));
                    }
                    let rows = plain(&blocks, RenderPrefs::default()).join("\n");
                    assert!(rows.contains(INTERRUPTED_MARKER), "{state}, {name}: {rows}");
                    assert!(rows.contains('✗'), "{state}, {name}: {rows}");
                    assert!(!rows.contains('✓'), "{state}, {name}: {rows}");
                    // One call keeps its own item line, which says so.
                    assert!(rows.contains("· interrupted"), "{rows}");
                }
            }
        }
    }
}

#[test]
fn interrupted_cell_children_are_counted_once_and_never_merged_away() {
    for expanded in [false, true] {
        // Opening a group does not open its cells; the cell opens itself.
        let mode = if expanded {
            DisplayMode::Expanded
        } else {
            DisplayMode::Collapsed
        };
        let mut head = compose_cell(mode, "cancelled", "2 tools · 2 failed");
        if let ScrollBlock::Tool { group_open, .. } = &mut head {
            *group_open = expanded;
        }
        let blocks = vec![
            head,
            compose_child("read_file", "cancelled", "a.rs"),
            compose_child("read_file", "interrupted", "b.rs"),
            ScrollBlock::System(INTERRUPTED_MARKER.into()),
            ScrollBlock::Assistant("Next answer.".into()),
        ];
        let rows = plain(&blocks, RenderPrefs::default()).join("\n");
        assert!(rows.contains("(2 interrupted)"), "{rows}");
        assert!(!rows.contains("failed"), "{rows}");
        assert!(!rows.contains('✓'), "{rows}");
        assert!(rows.contains(INTERRUPTED_MARKER), "{rows}");
        if expanded {
            assert!(rows.contains("a.rs") && rows.contains("b.rs"), "{rows}");
            assert!(!rows.contains("Explored"), "{rows}");
        }
    }
}

#[test]
fn activity_summary_distinguishes_failed_and_interrupted_calls() {
    let blocks = vec![
        tool("exec_command", "failed:exit 7", "false"),
        tool("exec_command", "cancelled", "sleep 60"),
        ScrollBlock::Assistant("Next answer.".into()),
    ];
    let rows = plain(&blocks, RenderPrefs::default()).join("\n");
    assert!(
        rows.contains("✗ Ran 2 commands (1 failed, 1 interrupted)  exit 7 ▸"),
        "{rows}"
    );
}

#[test]
fn activity_group_keeps_marker_after_completed_tool_before_compaction() {
    let mut compact = tool("compact", "done", "16.9k → 13.1k");
    if let ScrollBlock::Tool { kind, .. } = &mut compact {
        *kind = ActivityKind::Compact;
    }
    let blocks = vec![
        tool("custom_tool", "done", "completed action"),
        ScrollBlock::System(INTERRUPTED_MARKER.into()),
        compact,
    ];
    let rows = plain(&blocks, RenderPrefs::default()).join("\n");
    assert!(rows.contains(INTERRUPTED_MARKER), "{rows}");
}

#[test]
fn activity_group_closes_on_assistant_text_and_keeps_failures() {
    let blocks = vec![
        tool("read_file", "done", "a.rs"),
        tool("grep", "done", "needle"),
        tool("exec_command", "done", "true"),
        tool("exec_command", "failed:exit 7", "false"),
        ScrollBlock::Assistant("Finished.".into()),
    ];
    let rows = plain(&blocks, RenderPrefs::default());
    assert!(
        rows.iter()
            .any(|row| row.contains("Read 1 file, ran 1 search, ran 2 commands (1 failed)")),
        "{rows:?}"
    );
    assert!(rows.iter().any(|row| row.contains("exit 7")), "{rows:?}");
    assert!(!rows.iter().any(|row| row.contains("needle")), "{rows:?}");
}

#[test]
fn activity_group_seals_before_compaction_even_with_an_intervening_notice() {
    let mut compact = tool("compact", "done", "16.9k → 13.1k");
    if let ScrollBlock::Tool { kind, .. } = &mut compact {
        *kind = ActivityKind::Compact;
    }
    let blocks = vec![
        tool("custom_tool", "done", "first"),
        tool("custom_tool", "done", "second"),
        ScrollBlock::item("observation clipped"),
        compact,
    ];
    let rows = plain(&blocks, RenderPrefs::default());
    assert!(rows[0].contains("Custom tool ×2"), "{rows:?}");
    assert!(!rows.iter().any(|row| row.contains("clipped")), "{rows:?}");
    assert!(!rows.iter().any(|row| row.contains("first")), "{rows:?}");
}

#[test]
fn consecutive_read_only_rows_fold_into_one_group_row() {
    let mut blocks = vec![
        tool("read_file", "done", "a.rs"),
        tool("grep", "done", "query"),
        tool("read_file", "done", "b.rs"),
    ];
    assert_eq!(
        plain(&blocks, RenderPrefs::default()),
        ["  ✓ Read 2 files, ran 1 search  ▸"]
    );
    blocks.push(ScrollBlock::Assistant("Done.".into()));
    assert_eq!(
        plain(&blocks, RenderPrefs::default())[0],
        "  ✓ Read 2 files, ran 1 search  ▸"
    );
    if let ScrollBlock::Tool { group_open, .. } = &mut blocks[0] {
        *group_open = true;
    }
    let opened = plain(&blocks, RenderPrefs::default());
    assert_eq!(opened[0], "  ✓ Read 2 files, ran 1 search  ▾");
    assert_eq!(opened[1], "  └─  ✓ Explored 2 files, 1 search  ▸");
    let ungrouped = plain(
        &blocks,
        RenderPrefs {
            group_tool_calls: false,
            ..RenderPrefs::default()
        },
    );
    assert_eq!(ungrouped[0], "  ✓ Explored 2 files, 1 search  ▸");
}

#[test]
fn an_open_verb_group_agrees_on_estimated_height() {
    let mut blocks = vec![
        tool("read_file", "done", "a.rs"),
        tool("read_file", "done", "b.rs"),
        tool("read_file", "done", "c.rs"),
    ];
    if let ScrollBlock::Tool { group_open, .. } = &mut blocks[0] {
        *group_open = true;
    }
    let theme = crate::tui::theme::Theme::dark();
    let plans = plan_rows(&blocks, RenderPrefs::default());
    for (index, block) in blocks.iter().enumerate() {
        let exact = render::block_lines_with(
            block,
            &plans[index],
            &theme,
            100,
            RenderPrefs::default(),
            &blocks,
        )
        .len();
        let estimated =
            render::estimate_block_rows(block, &plans[index], 100, RenderPrefs::default(), &blocks);
        assert_eq!(
            exact, estimated,
            "block {index} is measured differently by the two projections: exact={exact} estimated={estimated}"
        );
    }
}

#[test]
fn groups_include_mutations_and_failures_but_not_assistant_prose() {
    let blocks = vec![
        tool("exec_command", "done", "true"),
        tool("exec_command", "failed:exit 2", "false"),
        tool("apply_patch", "done", "a.rs +12/-3"),
        ScrollBlock::Assistant("First note".into()),
        tool("read_file", "done", "a.rs"),
        ScrollBlock::Assistant("Second note".into()),
    ];
    let rows = plain(&blocks, RenderPrefs::default());
    assert!(
        rows.iter()
            .any(|row| row.contains("Ran 2 commands") || row.contains("ran 2 commands")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("(1 failed)") && row.contains("exit 2")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.to_lowercase().contains("edited 1 file (+12 −3)")),
        "{rows:?}"
    );
    assert!(rows.iter().any(|row| row.contains("Read a.rs")), "{rows:?}");
}

#[test]
fn each_read_only_verb_names_its_own_group() {
    assert_eq!(group_header(ToolVerb::Read, 3), "Read 3 files");
    assert_eq!(group_header(ToolVerb::Search, 4), "Searched 4 patterns");
    assert_eq!(group_header(ToolVerb::List, 2), "Listed 2 dirs");
    assert_eq!(group_header(ToolVerb::Fetch, 5), "Fetched 5 websites");
}

#[test]
fn thinking_streams_as_one_spinner_row_and_seals_with_a_caret() {
    let streaming = ScrollBlock::Thinking {
        location_key: "0:0".into(),
        text: "weighing the two shapes\nchecking the fold policy".into(),
        sealed: false,
        elapsed_ms: None,
        mode: DisplayMode::Collapsed,
        manual: false,
    };
    let rows = plain(&[streaming], RenderPrefs::default());
    assert_eq!(rows, ["  ⠼ Thinking…"]);

    let sealed = ScrollBlock::Thinking {
        location_key: "0:0".into(),
        text: "weighing the two shapes".into(),
        sealed: true,
        elapsed_ms: Some(4_100),
        mode: DisplayMode::Collapsed,
        manual: false,
    };
    assert_eq!(
        plain(std::slice::from_ref(&sealed), RenderPrefs::default()),
        vec!["  ◆ weighing the two shapes  4.1s".to_owned()]
    );

    // §5.5 `show thinking blocks` off removes the block entirely.
    assert_eq!(
        plain(
            &[sealed],
            RenderPrefs {
                show_thinking: false,
                ..RenderPrefs::default()
            }
        ),
        [] as [String; 0]
    );
}

#[test]
fn review_cell_footer_estimate_tracks_open_and_full_output() {
    let mut block = compose_cell(DisplayMode::Expanded, "done", "0 tools");
    if let ScrollBlock::Tool { view, .. } = &mut block {
        view.fetched = Some(std::sync::Arc::new(CallDetailOutcome::Detail {
            input: String::new(),
            output: (0..100)
                .map(|i| format!("row {i} with words"))
                .collect::<Vec<_>>()
                .join("\n"),
            input_truncated: false,
            output_truncated: true,
            source: plakous_host_protocol::views::CallDetailSource::Canonical,
            sources: Vec::new(),
        }));
    }
    let mut estimates = Vec::new();
    for (open, full) in [(false, false), (true, false), (true, true)] {
        if let ScrollBlock::Tool { view, .. } = &mut block {
            view.output_open = open;
            view.full_output = full;
        }
        let blocks = [block.clone()];
        let plan = plan_rows(&blocks, RenderPrefs::default());
        let estimate =
            render::estimate_block_rows(&blocks[0], &plan[0], 40, RenderPrefs::default(), &blocks);
        let exact = render::block_lines_with(
            &blocks[0],
            &plan[0],
            &crate::tui::Theme::dark(),
            40,
            RenderPrefs::default(),
            &blocks,
        )
        .len();
        assert!(estimate >= exact, "{open}/{full}: {estimate} < {exact}");
        estimates.push(estimate);
    }
    assert!(estimates[0] < estimates[1] && estimates[1] < estimates[2]);
    assert!(
        size_of::<CellTail>() <= 4 * size_of::<usize>(),
        "footer plans only carry indices/counts/rail"
    );
}

#[test]
fn review_thought_summary_skips_markdown_only_lines() {
    for (body, summary) in [
        ("```rust\nlet x = 1;\n```", "let x = 1;"),
        ("---\n**Title**", "Title"),
        ("<!-- comment -->\n# Heading", "Heading"),
        ("---", "---"),
        ("<!-- comment -->", "<!-- comment -->"),
        (
            "Checking Option<String> handling",
            "Checking Option<String> handling",
        ),
        (
            "Using Vec<T> and Result<T> here",
            "Using Vec<T> and Result<T> here",
        ),
        ("Arc<Mutex<State>> shared", "Arc<Mutex<State>> shared"),
        ("inline <b>bold</b>", "inline <b>bold</b>"),
        ("x < y", "x < y"),
        ("<b>bold</b>", "<b>bold</b>"),
        ("text <!-- comment --> here", "text <!-- comment --> here"),
        ("<div>block HTML</div>\n**Title**", "Title"),
    ] {
        assert_eq!(thinking_summary(body), summary);
    }
}

#[test]
fn opening_a_thought_keeps_its_entire_body() {
    let body = (0..50)
        .map(|i| format!("thought line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(thinking_detail_rows(&body, DisplayMode::Expanded).len(), 50);
    assert_eq!(
        thinking_detail_rows(&body, DisplayMode::Expanded)[25],
        "thought line 25"
    );
}

#[test]
fn collapsed_thoughts_show_clean_summaries_and_single_lines_do_not_toggle() {
    for (body, summary) in [
        ("**Bold title**", "Bold title"),
        ("# Heading", "Heading"),
        ("- item", "item"),
        ("`code`", "code"),
        ("> __quote__", "quote"),
        ("\n  *A*   _title_  \nbody", "A title"),
        ("你好 👋 summary", "你好 👋 summary"),
    ] {
        let block = ScrollBlock::Thinking {
            location_key: "t".into(),
            text: body.into(),
            sealed: true,
            elapsed_ms: Some(2_100),
            mode: DisplayMode::Collapsed,
            manual: false,
        };
        let rows = layout_blocks(std::slice::from_ref(&block), 80)
            .plain
            .join("\n");
        assert!(rows.contains(&format!("◆ {summary}")), "{rows}");
        assert!(rows.contains("2.1s"), "{rows}");
        assert_eq!(
            rows.contains('▸'),
            body.trim().lines().count() > 1,
            "{rows}"
        );
        let narrow = layout_blocks(&[block], 18).plain;
        assert!(narrow.iter().all(|row| row.width() <= 18), "{narrow:?}");
    }
}

#[test]
fn opening_a_cell_keeps_outline_before_source_and_output_closed() {
    let mut head = compose_cell(DisplayMode::Expanded, "done", "1 tools");
    if let ScrollBlock::Tool { view, .. } = &mut head {
        view.fetched = Some(std::sync::Arc::new(CallDetailOutcome::Detail {
            input: "source".into(),
            output: "CELL OUTPUT".into(),
            input_truncated: false,
            output_truncated: false,
            source: plakous_host_protocol::views::CallDetailSource::Canonical,
            sources: Vec::new(),
        }));
    }
    let rows = layout_blocks(&[head, compose_child("exec_command", "done", "pwd")], 100).plain;
    let call = rows.iter().position(|r| r.contains("$ pwd")).unwrap();
    let source = rows.iter().position(|r| r.contains("const out")).unwrap();
    assert!(call < source, "{rows:?}");
    assert!(rows.iter().any(|r| r.contains("output ▸")), "{rows:?}");
    assert!(!rows.iter().any(|r| r.contains("CELL OUTPUT")), "{rows:?}");
}

fn compose_cell(mode: DisplayMode, state: &str, subject: &str) -> ScrollBlock {
    ScrollBlock::Tool {
        call_id: Some("cell-1".into()),
        parent_call_id: None,
        cell_id: Some("cell-1".into()),
        kind: ActivityKind::Compose,
        name: "exec_js".into(),
        state: state.to_owned(),
        subject: Some(subject.to_owned()),
        detail: Some(
            "const out = await exec_command({ cmd: \"cargo test\" });\nawait apply_patch({ path: \"ui.rs\", patch });"
                .to_owned(),
        ),
        mode,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: ItemView::default(),
    }
}

fn compose_child(name: &str, state: &str, subject: &str) -> ScrollBlock {
    ScrollBlock::Tool {
        call_id: Some(format!("child-{subject}")),
        parent_call_id: Some("cell-1".into()),
        cell_id: Some("cell-1".into()),
        kind: ActivityKind::Tool,
        name: name.to_owned(),
        state: state.to_owned(),
        subject: Some(subject.to_owned()),
        detail: None,
        mode: DisplayMode::Collapsed,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: ItemView::default(),
    }
}

/// Codex Code Mode shape: the cell yielded after three
/// children, the `wait` continuation ran two more. The host projects the
/// marker as the wait row's parent, so the rail stays one rail in
/// transcript order instead of breaking around a flat `wait` row.
#[test]
fn a_wait_continuation_keeps_the_compose_rail_unbroken() {
    let blocks = vec![
        compose_cell(DisplayMode::Expanded, "done", "2 tools"),
        compose_child("exec_command", "done", "echo first"),
        compose_child("exec_command", "done", "echo second"),
    ];
    let rows = plain(&blocks, RenderPrefs::default());
    assert_eq!(
        rows.iter().filter(|row| row.contains("CodeMode")).count(),
        1
    );
    assert!(rows.iter().any(|row| row.contains("$ echo first")));
    assert!(rows.iter().any(|row| row.contains("$ echo second")));
}

#[test]
fn host_counted_script_failures_remain_visible_without_child_rows() {
    let blocks = vec![
        compose_cell(DisplayMode::Collapsed, "done", "3 tools · 1 failed"),
        ScrollBlock::Assistant("Finished.".into()),
    ];
    let rows = plain(&blocks, RenderPrefs::default());
    assert!(
        rows[0].contains("✗") && rows[0].contains("1 failed"),
        "{rows:?}"
    );
}

#[test]
fn code_cells_expand_source_and_actions_at_one_level() {
    for mode in [DisplayMode::Collapsed, DisplayMode::Expanded] {
        let blocks = vec![
            compose_cell(mode, "done", "2 tools"),
            compose_child("exec_command", "done", "cargo test"),
            compose_child("read_file", "done", "a.rs"),
        ];
        let rows = plain(&blocks, RenderPrefs::default());
        let caret = if mode == DisplayMode::Expanded {
            "▾"
        } else {
            "▸"
        };
        assert_eq!(rows[0], format!("  ✓ CodeMode · 2 calls  {caret}"));
        assert_eq!(
            rows.iter().any(|row| row.contains("const out")),
            mode == DisplayMode::Expanded,
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("$ cargo test")),
            "{rows:?}"
        );
    }
}

/// bodies (and Code Mode JS) must not stack above the composer.
#[test]
fn expanded_code_cell_children_never_show_payloads() {
    // Opening a cell opens the cell, not its calls: each opens on its own.
    let mut child = compose_child("exec_command", "running", "ls -1");
    if let ScrollBlock::Tool { detail, .. } = &mut child {
        *detail = Some("RAW CHILD RESULT".into());
    }
    let blocks = vec![
        compose_cell(DisplayMode::Expanded, "running", "1 tools"),
        child,
    ];
    let rows = plain(&blocks, RenderPrefs::default());
    assert!(rows.iter().any(|row| row.contains("$ ls -1")), "{rows:?}");
    assert!(
        !rows.iter().any(|row| row.contains("RAW CHILD RESULT")),
        "{rows:?}"
    );
}

#[test]
fn a_lineage_the_renderer_was_not_given_stays_flat() {
    // Same names, same order, no `parent_call_id`: never a guess (§5.8).
    let blocks = vec![
        compose_cell(DisplayMode::Collapsed, "running", "0 tools"),
        tool("exec_command", "done:1.0s", "cargo test"),
    ];
    let rows = plain(&blocks, RenderPrefs::default());
    assert_eq!(rows.len(), 2);
    assert!(!rows[1].contains('├'), "{rows:?}");
    assert!(!rows[1].contains('└'), "{rows:?}");
}

#[test]
fn agent_actions_merge_working_and_finished_counts() {
    let blocks = vec![
        tool("spawn_agent", "done", "review"),
        tool("spawn_agent", "done", "edit"),
        tool("spawn_agent", "done", "test"),
    ];
    let ungrouped = RenderPrefs {
        group_tool_calls: false,
        ..RenderPrefs::default()
    };
    assert_eq!(plain(&blocks, ungrouped), ["  ⠼ 3 agents working  ▸"]);
    assert_eq!(
        plain(&blocks, RenderPrefs::default()),
        ["  ✓ Spawned 3 agents  ▸"]
    );
    let blocks = vec![
        tool("wait_agent", "done", "review"),
        tool("wait_agent", "done", "edit"),
        tool("wait_agent", "done", "test"),
    ];
    assert_eq!(plain(&blocks, ungrouped), ["  ✓ 3 agents finished  ▸"]);
}

#[test]
fn approval_rows_state_the_decision_and_then_its_resolution() {
    let pending = ScrollBlock::Approval {
        approval_id: "approval-1".into(),
        tool_name: "exec_command".into(),
        subject: Some("rm -rf build".into()),
        prompt: Some("this command deletes files".into()),
        resolution: None,
    };
    let rows = plain(&[pending], RenderPrefs::default());
    assert_eq!(
        rows[0],
        "  ◆ Approval  exec command rm -rf build · awaiting decision · [allow] Alt+A · [deny] Alt+D"
    );
    assert_eq!(rows[1], "  └ this command deletes files");

    let resolved = ScrollBlock::Approval {
        approval_id: "approval-1".into(),
        tool_name: "exec_command".into(),
        subject: None,
        prompt: None,
        resolution: Some("denied".into()),
    };
    assert_eq!(
        plain(&resolved_blocks(resolved), RenderPrefs::default())[0],
        "  ⊘ Approval  exec command · denied"
    );
}

fn resolved_blocks(block: ScrollBlock) -> Vec<ScrollBlock> {
    vec![block]
}

#[test]
fn tool_plain_collapses_lifecycle() {
    assert_eq!(format_tool_plain("run_process", "running", None), "  ◌ Run");
    assert_eq!(format_tool_plain("run_process", "done", None), "  ✓ Run");
    assert_eq!(format_tool_plain("read_file", "start", None), "  ◌ Read");
    assert_eq!(
        format_tool_plain("read_file", "done", Some("src/lib.rs")),
        "  ✓ Read src/lib.rs"
    );
    assert_eq!(
        format_tool_plain("search_repository", "running", Some("ToolLine")),
        "  ◌ Search ToolLine"
    );
    assert_eq!(
        format_tool_summary("read_file", "done", Some("src/lib.rs"), true),
        "  ✓ Read src/lib.rs"
    );
    assert_eq!(display_tool_title("apply_patch"), "Edit");
    assert_eq!(display_tool_title("update_plan"), "Plan");
    assert_eq!(display_tool_title("exec_command"), "Run");
    assert_eq!(display_tool_title("web__run"), "Fetch");
    assert_eq!(
        format_tool_plain("exec_command", "failed", None),
        "  × Run · failed"
    );
    assert_eq!(
        format_tool_plain("custom_tool", "unknown", None),
        "  • Custom tool · unknown"
    );
    assert_eq!(display_tool_title("custom_tool"), "Custom tool");
}

#[test]
fn collapsed_rows_keep_the_subject_in_the_plain_projection() {
    assert_eq!(
        format_tool_plain("exec_command", "done:12ms", Some("cargo test")),
        "  ✓ Run cargo test · 12ms"
    );
}

#[test]
fn subject_budget_truncates_commands_right_and_paths_left() {
    assert_eq!(budget_tool_subject("cargo test", 20), "cargo test");
    assert_eq!(budget_tool_subject("cargo test --all", 10), "cargo tes…");
    assert_eq!(
        budget_tool_subject("crates/plak/tui/src/blocks.rs", 12),
        "…c/blocks.rs"
    );
    assert_eq!(budget_tool_subject("cargo", 1), "…");
    assert_eq!(budget_tool_subject("cargo", 0), "");
    // Wide graphemes are budgeted by display cells, never by chars.
    assert_eq!(budget_tool_subject("界界界界", 5), "界界…");
}

#[test]
fn a_compact_row_is_a_header_with_before_and_after_sizes() {
    assert_eq!(
        format_tool_row(
            "compact",
            "done:29.3s",
            ActivityKind::Compact,
            Some("199.7k → 12.4k"),
            DisplayMode::Collapsed,
            &RowPlan::default(),
        ),
        "  ✓ Compact  199.7k → 12.4k · 29.3s"
    );
    assert_eq!(
        format_tool_row(
            "compact",
            "overflow:29.3s",
            ActivityKind::Compact,
            Some("199.7k → 178.1k · still over capacity"),
            DisplayMode::Collapsed,
            &RowPlan::default(),
        ),
        "  × Compact  199.7k → 178.1k · still over capacity · 29.3s"
    );
    assert_eq!(
        tool_verb("compact", ActivityKind::Compact),
        ToolVerb::Compact
    );
}

#[test]
fn detail_window_names_the_lines_it_hid() {
    for count in [100, 31] {
        let output = (1..=count)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut block = tool("read_file", "done", "a.rs");
        if let ScrollBlock::Tool { mode, view, .. } = &mut block {
            *mode = DisplayMode::Expanded;
            view.fetched = Some(std::sync::Arc::new(CallDetailOutcome::Detail {
                input: String::new(),
                output,
                input_truncated: false,
                output_truncated: false,
                source: plakous_host_protocol::views::CallDetailSource::Canonical,
                sources: Vec::new(),
            }));
        }
        let rows = layout_blocks(&[block], 80).plain;
        assert_eq!(
            rows.iter().filter(|row| row.contains("│ line ")).count(),
            if count == 100 { 30 } else { 31 }
        );
        assert!(
            rows.iter()
                .any(|row| row.contains(&format!("│ line {count}")))
        );
        assert_eq!(
            rows.iter().any(|row| row.contains("… 70 lines hidden ▸")),
            count == 100
        );
    }
}

/// Web search, X search, fetch and MCP calls each read as one action
/// row, gerund while running and past tense once done, at every width.
#[test]
fn web_and_mcp_rows_read_as_their_actions() {
    let cases = [
        (
            "web_search",
            "\"rust 1.99\"",
            "Searching the web \"rust 1.99\"",
            "Searched the web \"rust 1.99\"",
        ),
        (
            "WebSearch",
            "\"rust 1.99\"",
            "Searching the web \"rust 1.99\"",
            "Searched the web \"rust 1.99\"",
        ),
        (
            "x_search",
            "\"plak\"",
            "Searching X \"plak\"",
            "Searched X \"plak\"",
        ),
        (
            "web_fetch",
            "docs.rs",
            "Fetching docs.rs",
            "Fetched docs.rs",
        ),
        ("WebFetch", "docs.rs", "Fetching docs.rs", "Fetched docs.rs"),
        (
            "fetch_url",
            "docs.rs",
            "Fetching docs.rs",
            "Fetched docs.rs",
        ),
        (
            "image_gen",
            "art/fox.png (1024x1024)",
            "Generating image art/fox.png (1024x1024)",
            "Generated image art/fox.png (1024x1024)",
        ),
        (
            "mcp__docs__search",
            "query=\"install\"",
            "mcp:docs search query=\"install\"",
            "mcp:docs search query=\"install\"",
        ),
    ];
    for (name, subject, running, done) in cases {
        for (state, label) in [("running", running), ("done:1.2s", done)] {
            let wide = lines(&tool(name, state, subject), 100, RenderPrefs::default());
            assert!(wide[0].contains(label), "{name} {state}: {:?}", wide[0]);
            let narrow = lines(&tool(name, state, subject), 30, RenderPrefs::default());
            assert!(narrow[0].width() <= 30, "{name} {state}: {:?}", narrow[0]);
            assert!(
                narrow[0].trim_end().ends_with('▸'),
                "{name}: {:?}",
                narrow[0]
            );
            let head: String = label.chars().take(8).collect();
            assert!(narrow[0].contains(&head), "{name} {state}: {:?}", narrow[0]);
        }
    }
    assert_eq!(
        display_tool_title_for("mcp__git_hub__list_issues", ActivityKind::Tool),
        "mcp:git_hub list_issues"
    );
    // A name outside the exposed MCP grammar stays a generic tool.
    assert_eq!(
        tool_verb("mcp__Docs__x", ActivityKind::Tool),
        ToolVerb::Generic
    );
}

/// Rows stored before the web rows existed render exactly as they did: a
/// stored fetch keeps its stored whole-URL subject, and a stored MCP row
/// (labelled by its exposed name) reads as an MCP call.
#[test]
fn stored_web_and_mcp_rows_render_from_their_stored_subjects() {
    let row = |name: &str, subject: &str| {
        ScrollBlock::from_row(TranscriptRow::Tool(ToolItem {
            call_id: format!("{name}-1"),
            name: name.to_owned(),
            exposed_name: None,
            kind: ActivityKind::Tool,
            state: "done".to_owned(),
            subject: Some(subject.to_owned()),
            detail: None,
            started_at_ms: Some(1_000),
            finished_at_ms: Some(2_200),
            parent_call_id: None,
            cell_id: None,
            wait: None,
        }))
        .unwrap()
    };
    let fetched = lines(
        &row("web_fetch", "https://a.test/x?q=1"),
        100,
        RenderPrefs::default(),
    );
    assert!(
        fetched[0].contains("✓ Fetched https://a.test/x?q=1"),
        "{:?}",
        fetched[0]
    );
    let mcp = lines(
        &row("mcp__docs__search", "query=\"install\""),
        100,
        RenderPrefs::default(),
    );
    assert!(
        mcp[0].contains("✓ mcp:docs search query=\"install\""),
        "{:?}",
        mcp[0]
    );
}

fn expanded_with(name: &str, output: String, sources: Vec<SourceLink>) -> ScrollBlock {
    let mut block = tool(name, "done", "\"rust\"");
    if let ScrollBlock::Tool { mode, view, .. } = &mut block {
        *mode = DisplayMode::Expanded;
        view.fetched = Some(std::sync::Arc::new(CallDetailOutcome::Detail {
            input: "{\"query\":\"rust\"}".to_owned(),
            output,
            input_truncated: false,
            output_truncated: false,
            source: plakous_host_protocol::views::CallDetailSource::Canonical,
            sources,
        }));
    }
    block
}

/// An open search or fetch row lists the host-parsed sources, title then
/// URL, above the result; other tools never show a sources list.
#[test]
fn expanded_web_rows_list_their_sources() {
    let sources = vec![
        SourceLink {
            title: "Rust Blog".to_owned(),
            url: "https://blog.rust-lang.org/a".to_owned(),
        },
        SourceLink {
            title: String::new(),
            url: "https://example.org/n".to_owned(),
        },
    ];
    for name in [
        "web_search",
        "WebSearch",
        "x_search",
        "web_fetch",
        "fetch_url",
    ] {
        let rows = layout_blocks(
            &[expanded_with(name, "answer".to_owned(), sources.clone())],
            100,
        )
        .plain;
        let at = |needle: &str| rows.iter().position(|row| row.contains(needle));
        let header = at("sources").unwrap_or_else(|| panic!("{name}: {rows:?}"));
        let first = at("Rust Blog  https://blog.rust-lang.org/a").expect("titled source");
        let second = at("https://example.org/n").expect("untitled source");
        let answer = at("answer").expect("result");
        assert!(
            header < first && first < second && second < answer,
            "{rows:?}"
        );
    }
    let rows = layout_blocks(&[expanded_with("read_file", "x".to_owned(), sources)], 100).plain;
    assert!(
        !rows.iter().any(|row| row.contains("Rust Blog")),
        "{rows:?}"
    );
}

/// A folded output window keeps a `full output:` spill line visible even
/// when it sits in the hidden middle.
#[test]
fn a_folded_output_keeps_its_spill_line_visible() {
    let mut lines_out: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
    lines_out[49] = "full output: /state/jobs/s/mcp-c1.txt".to_owned();
    let rows = layout_blocks(
        &[expanded_with(
            "mcp__docs__search",
            lines_out.join("\n"),
            Vec::new(),
        )],
        100,
    )
    .plain;
    assert!(
        rows.iter()
            .any(|row| row.contains("full output: /state/jobs/s/mcp-c1.txt")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("… 69 lines hidden ▸")),
        "{rows:?}"
    );
}

/// `0ms` printed for `Read 3 files`, `Edit src/lib.rs +1/-1`,
/// `list agents` and `Compose 0 tools` — calls that plainly did work. A
/// duration the row cannot resolve is reported as the floor it can, and a
/// duration it did measure is still printed verbatim.
#[test]
fn a_call_that_did_work_never_paints_a_zero_millisecond_duration() {
    assert_eq!(format_tool_elapsed(Duration::ZERO), "<1ms");
    assert_eq!(format_tool_elapsed(Duration::from_micros(400)), "<1ms");
    assert_eq!(format_tool_elapsed(Duration::from_millis(1)), "1ms");
    assert_eq!(format_tool_elapsed(Duration::from_millis(81)), "81ms");
    assert_eq!(format_tool_elapsed(Duration::from_millis(1200)), "1.2s");

    // The row carries it exactly as any other measured elapsed, and a
    // later bare `done` re-emit does not clear it (§5.8).
    let row = format_tool_plain("read_file", "done:<1ms", Some("a.txt"));
    assert_eq!(row, "  ✓ Read a.txt · <1ms");
    assert_eq!(
        terminal_state_keeping_duration("done:<1ms", "done".to_owned()),
        "done:<1ms"
    );
}

/// §5.8: the Compose header is the host-counted tally alone; the program
/// is one activation away behind the rail. `1 tools` is a count with
/// the plural glued on.
#[test]
fn a_compose_header_is_its_host_counted_tally() {
    assert_eq!(
        row_subject(Some("3 tools · 1 failed")).as_deref(),
        Some("3 tools · 1 failed")
    );
    assert_eq!(row_subject(Some("1 tools")).as_deref(), Some("1 tool"));
    assert_eq!(row_subject(Some("1 files")).as_deref(), Some("1 file"));
    assert_eq!(row_subject(Some("")), None);
    assert_eq!(row_subject(None), None);
    assert_eq!(compose_program_language("exec"), Some("javascript"));
    assert_eq!(compose_program_language("exec_js"), Some("javascript"));
    assert_eq!(compose_program_language("exec_py"), Some("python"));
    assert_eq!(compose_program_language("wait_py"), Some("python"));
    assert_eq!(compose_program_language("apply_patch"), None);
}

/// Wrapped composition source keeps its rail within the terminal width.
#[test]
fn a_clipped_compose_source_row_keeps_its_rail() {
    let theme = crate::tui::theme::Theme::dark();
    let block = ScrollBlock::Tool {
        call_id: Some("cell-1".into()),
        parent_call_id: None,
        cell_id: Some("cell-1".into()),
        kind: ActivityKind::Compose,
        name: "exec_js".into(),
        state: "done:1.2s".into(),
        subject: Some("1 tools".into()),
        detail: Some(format!(
            "const patch = \"{}\";",
            "*** Begin Patch *** Update File: src/lib.rs ".repeat(4)
        )),
        mode: DisplayMode::Expanded,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: ItemView::default(),
    };
    let lines = render::block_lines(
        &block,
        &plan_rows(std::slice::from_ref(&block), RenderPrefs::default())[0],
        &theme,
        80 - 4,
        RenderPrefs::default(),
    );
    let rows = lines
        .iter()
        .map(|line| line.to_string().trim_end().to_owned())
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>();
    assert!(rows.len() > 3, "the source clipped: {rows:?}");
    assert_eq!(rows[1], "    │ ┌ javascript", "{rows:?}");
    for row in &rows[2..] {
        assert!(
            row.starts_with("    │ "),
            "every detail row, wrapped or not, starts on the rail: {row:?}"
        );
        assert!(row.width() <= 76, "{row:?}");
    }
}

#[test]
fn wrapped_group_members_end_the_last_rail() {
    let block = tool(
        "exec_command",
        "done",
        "cargo nextest run --locked --workspace --all-features",
    );
    for last in [false, true] {
        let plan = RowPlan {
            group_child: Some(last),
            ..RowPlan::default()
        };
        let rows = render::block_lines(
            &block,
            &plan,
            &crate::tui::Theme::dark(),
            24,
            RenderPrefs::default(),
        );
        assert!(rows.len() > 1);
        assert!(rows[0].to_string().starts_with(row_indent(&plan)));
        let continuation = if last { "      " } else { "  │   " };
        for row in rows
            .iter()
            .skip(1)
            .filter(|row| !row.to_string().trim().is_empty())
        {
            assert!(row.to_string().starts_with(continuation), "{row:?}");
            assert!(row.width() <= 24);
        }
    }
}

#[test]
fn narrow_command_rows_stay_one_line_and_drop_meta_before_the_subject() {
    let block = tool(
        "exec_command",
        "done:1s",
        "cargo nextest run --locked --workspace",
    );
    let rows = render::block_lines(
        &block,
        &RowPlan::default(),
        &crate::tui::Theme::dark(),
        20,
        RenderPrefs::default(),
    );
    let text = rows
        .iter()
        .flat_map(|row| row.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert_eq!(text.trim_end(), "  ✓ $ cargo nexte… ▸");
    assert!(rows.iter().all(|row| row.width() <= 20));
    let wide = render::block_lines(
        &block,
        &RowPlan::default(),
        &crate::tui::Theme::dark(),
        60,
        RenderPrefs::default(),
    );
    assert!(wide[0].to_string().contains("--workspace"));
    assert!(wide[0].to_string().ends_with("1s ▸"), "{}", wide[0]);
    let medium = render::block_lines(
        &block,
        &RowPlan::default(),
        &crate::tui::Theme::dark(),
        30,
        RenderPrefs::default(),
    );
    let medium = medium[0].to_string();
    assert!(medium.ends_with(" 1s ▸"), "{medium}");
    assert!(medium.contains('…'), "{medium}");
}

#[test]
fn narrow_collapsed_tool_row_surrenders_the_subject_before_the_row() {
    let theme = crate::tui::theme::Theme::dark();
    let block = ScrollBlock::Tool {
        call_id: None,
        name: "read_file".into(),
        state: "done:1.2s".into(),
        subject: Some("crates/plak/tui/src/blocks.rs".into()),
        detail: None,
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: ItemView::default(),
        mode: DisplayMode::Collapsed,
    };
    for width in 8..=48 {
        let lines = render::block_lines(
            &block,
            &RowPlan::default(),
            &theme,
            width,
            RenderPrefs::default(),
        );
        assert!(
            lines[0].width() <= width,
            "row overflowed at width {width}: {}",
            lines[0]
        );
    }
}

#[test]
fn user_render_fills_the_row_with_the_prompt_band() {
    let theme = crate::tui::theme::Theme::dark();
    let lines = render::block_lines(
        &ScrollBlock::user("hello".into()),
        &RowPlan::default(),
        &theme,
        24,
        RenderPrefs::default(),
    );
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].style.bg, Some(theme.user_bg));
}

#[test]
fn multiline_user_render_uses_one_prompt_glyph() {
    let theme = crate::tui::theme::Theme::dark();
    let lines = render::block_lines(
        &ScrollBlock::user("first line wraps into a second line".into()),
        &RowPlan::default(),
        &theme,
        16,
        RenderPrefs::default(),
    );
    assert!(lines.len() > 1);
    assert_eq!(lines[0].spans[0].content, PROMPT_PREFIX);
    assert!(
        lines[1..]
            .iter()
            .all(|line| line.spans[0].content.as_ref() == "  ")
    );
}

#[test]
fn wrap_plain_uses_cells_and_preserves_whitespace() {
    assert_eq!(
        wrap_plain("界🙂e\u{301}  x\n", 4),
        vec!["界🙂", "e\u{301}  x", ""]
    );
}

#[test]
fn wrap_plain_moves_fitting_words_and_grapheme_splits_only_overlong_words() {
    assert_eq!(wrap_plain("go home", 5), ["go ", "home"]);
    assert_eq!(
        wrap_plain("go e\u{301}界🙂z", 5),
        ["go ", "e\u{301}界🙂", "z"]
    );
}

#[test]
fn activity_rows_keep_their_rail_on_one_cut_line() {
    let block = ScrollBlock::Tool {
        call_id: None,
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        name: "read_file".into(),
        state: "done".into(),
        subject: Some("a very long path with many words and a filename.rs".into()),
        detail: None,
        mode: DisplayMode::Collapsed,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: ItemView::default(),
    };
    let theme = crate::tui::theme::Theme::dark();
    let plan = RowPlan {
        group_child: Some(false),
        ..RowPlan::default()
    };
    let rows = render::block_lines(&block, &plan, &theme, 24, RenderPrefs::default());
    let plain_rows = block_plain_lines(
        &block,
        &plan,
        24,
        RenderPrefs::default(),
        std::slice::from_ref(&block),
    );
    let styled_rows: Vec<_> = rows
        .iter()
        .map(|row| row.to_string().trim_end().to_owned())
        .filter(|row| !row.is_empty())
        .collect();
    assert_eq!(plain_rows, styled_rows);
    assert_eq!(styled_rows.len(), 1, "{styled_rows:?}");
    assert!(styled_rows[0].starts_with("  ├─  ✓ Read a very"));
    assert!(styled_rows[0].ends_with("… ▸"), "{styled_rows:?}");
    assert_eq!(rows[0].width(), 24);
}

#[test]
fn thinking_body_keeps_hanging_indent_when_wrapped() {
    let block = ScrollBlock::Thinking {
        location_key: "0:0".into(),
        text: "weighing the two shapes\nagainst the fold policy for a long thought".into(),
        sealed: true,
        elapsed_ms: Some(4_100),
        mode: DisplayMode::Expanded,
        manual: false,
    };
    let rows = layout_blocks(std::slice::from_ref(&block), 24).plain;
    assert!(rows[0].starts_with("  ◆ weighing"), "{rows:?}");
    assert!(rows[0].ends_with('▾'), "{rows:?}");
    let body: Vec<&String> = rows
        .iter()
        .skip(1)
        .filter(|row| !row.trim().is_empty())
        .collect();
    assert!(body.len() > 1, "the thought must wrap: {rows:?}");
    for row in &body {
        assert!(
            row.starts_with("    "),
            "continuation lost the hanging indent: {row:?} in {rows:?}"
        );
    }

    let theme = crate::tui::theme::Theme::dark();
    let styled = render::block_lines(
        &block,
        &RowPlan::default(),
        &theme,
        24,
        RenderPrefs::default(),
    );
    let styled_body: Vec<String> = styled
        .iter()
        .skip(1)
        .map(ToString::to_string)
        .filter(|row| !row.trim().is_empty())
        .collect();
    assert!(styled_body.len() > 1, "styled thought must wrap");
    for row in &styled_body {
        assert!(
            row.starts_with("    "),
            "styled continuation lost the hanging indent: {row:?}"
        );
    }
}

#[test]
fn assistant_markdown_tables_do_not_wrap_at_the_pipe() {
    let source = concat!(
        "| Account | Auth | Models | Composition |\n",
        "| --- | --- | --- | --- |\n",
        "| grok | OAuth, paste code | grok-4.6 | direct |\n",
        "| codex | OAuth, loopback | gpt-6-astra | native-ptc |\n",
    );
    let theme = crate::tui::theme::Theme::dark();
    let styled = render::block_lines(
        &ScrollBlock::Assistant(source.to_owned()),
        &RowPlan::default(),
        &theme,
        64,
        RenderPrefs::default(),
    );
    let rows: Vec<String> = styled.iter().map(ToString::to_string).collect();
    let table: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains('│') || row.contains('┼'))
        .collect();
    assert!(table.len() >= 3, "expected a laid-out table: {rows:?}");
    let pipes = table
        .iter()
        .filter(|row| row.contains('│'))
        .map(|row| row.matches('│').count())
        .collect::<Vec<_>>();
    assert!(
        pipes.iter().all(|n| *n == 5),
        "a wrap at the pipe dropped a column: {table:?}"
    );
}

#[test]
fn no_sticky_at_top() {
    let blocks = vec![ScrollBlock::user("hi".into())];
    let layout = layout_blocks(&blocks, 40);
    assert!(sticky_user_lines(&blocks, &layout, 0, 2, 40).is_none());
}

#[test]
fn user_render_has_gutter_not_you_prefix() {
    let lines = wrap_plain("hello", 40);
    assert_eq!(lines, vec!["hello".to_owned()]);
    let layout = layout_blocks(&[ScrollBlock::user("hello".into())], 40);
    assert!(layout.plain[0].starts_with("┃ "));
    assert!(!layout.plain[0].contains("you"));
}

#[test]
fn distinct_block_changes_merge_to_structural() {
    assert_eq!(
        TranscriptChange::Block(1).merge(TranscriptChange::Block(2)),
        TranscriptChange::Structural
    );
}

/// The estimated and exact projections give a Compose the same
/// height, so nothing settles from two blank rows to one on a repaint.
///
/// The estimate charged every tool block a trailing blank; the paint path
/// gives one only to a plain tool row, a collapsed head, or the last child
/// of an open Compose.
#[test]
fn the_estimated_and_exact_projections_agree_on_a_compose_block() {
    use crate::tui::theme::Theme;

    let child = |name: &str| ScrollBlock::Tool {
        call_id: None,
        name: name.into(),
        state: "done".into(),
        subject: Some("pwd".into()),
        detail: Some("/workspace".into()),
        parent_call_id: Some("cell-1".into()),
        cell_id: Some("cell-1".into()),
        kind: ActivityKind::Tool,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: ItemView::default(),
        mode: DisplayMode::Collapsed,
    };
    let blocks = vec![
        ScrollBlock::Assistant("intro".into()),
        ScrollBlock::Tool {
            call_id: Some("cell-1".into()),
            name: "exec_js".into(),
            state: "done".into(),
            subject: None,
            detail: Some("const x = 1;".into()),
            parent_call_id: None,
            cell_id: Some("cell-1".into()),
            kind: ActivityKind::Tool,
            manual: false,
            group_open: false,
            wait: None,
            started_at_ms: None,
            finished_at_ms: None,
            view: ItemView::default(),
            mode: DisplayMode::Collapsed,
        },
        child("exec_command"),
        child("exec_command"),
        ScrollBlock::Assistant("outro".into()),
    ];

    let theme = Theme::dark();
    let plans = plan_rows(&blocks, RenderPrefs::default());
    for (index, block) in blocks.iter().enumerate() {
        let exact = render::block_lines_with(
            block,
            &plans[index],
            &theme,
            100,
            RenderPrefs::default(),
            &blocks,
        )
        .len();
        let estimated =
            render::estimate_block_rows(block, &plans[index], 100, RenderPrefs::default(), &blocks);
        assert_eq!(
            exact, estimated,
            "block {index} is measured differently by the two projections"
        );
    }
}

#[test]
fn tool_expand_block_splice_does_not_full_rebuild() {
    use crate::tui::theme::Theme;

    let blocks = vec![
        ScrollBlock::Assistant("intro".into()),
        ScrollBlock::Tool {
            call_id: None,
            name: "read_file".into(),
            state: "done".into(),
            subject: Some("src/lib.rs".into()),
            detail: Some("line one\nline two\nline three".into()),
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            manual: false,
            group_open: false,
            wait: None,
            started_at_ms: None,
            finished_at_ms: None,
            view: ItemView::default(),
            mode: DisplayMode::Collapsed,
        },
        ScrollBlock::Assistant("outro".into()),
    ];
    let tool_idx = 1;
    let mut cache = TranscriptRenderCache::default();
    let _ = cache.get(
        1,
        &blocks,
        &Theme::dark(),
        80,
        TranscriptChange::Structural,
        RenderPrefs::default(),
    );
    assert_eq!(cache.rebuilds(), 1);

    let mut expanded_blocks = blocks.clone();
    if let ScrollBlock::Tool { mode, .. } = &mut expanded_blocks[tool_idx] {
        *mode = DisplayMode::Expanded;
    }
    let collapsed_len = cache
        .get(
            1,
            &blocks,
            &Theme::dark(),
            80,
            TranscriptChange::Unchanged,
            RenderPrefs::default(),
        )
        .lines
        .len();
    let expanded = cache.get(
        2,
        &expanded_blocks,
        &Theme::dark(),
        80,
        TranscriptChange::Block(tool_idx),
        RenderPrefs::default(),
    );
    assert!(
        expanded.lines.len() > collapsed_len,
        "expanded tool should add detail rows"
    );
    assert_eq!(
        cache.rebuilds(),
        1,
        "tool expand should splice, not rebuild"
    );
}

#[test]
fn thousand_block_projection_is_reused_until_revision_or_width_changes() {
    use crate::tui::theme::Theme;

    let blocks: Vec<_> = (0..1_000)
        .map(|i| ScrollBlock::Assistant(format!("response {i}: cached transcript row")))
        .collect();
    let mut cache = TranscriptRenderCache::default();
    let first_len = cache
        .get(
            7,
            &blocks,
            &Theme::dark(),
            80,
            TranscriptChange::Structural,
            RenderPrefs::default(),
        )
        .lines
        .len();
    assert!(first_len >= 1_000);
    assert_eq!(cache.rebuilds(), 1);

    let second_len = cache
        .get(
            7,
            &blocks,
            &Theme::dark(),
            80,
            TranscriptChange::Unchanged,
            RenderPrefs::default(),
        )
        .lines
        .len();
    assert_eq!(second_len, first_len);
    assert_eq!(cache.rebuilds(), 1, "idle redraw must reuse projection");

    let _ = cache.get(
        8,
        &blocks,
        &Theme::dark(),
        80,
        TranscriptChange::Structural,
        RenderPrefs::default(),
    );
    let _ = cache.get(
        8,
        &blocks,
        &Theme::dark(),
        79,
        TranscriptChange::Unchanged,
        RenderPrefs::default(),
    );
    assert_eq!(cache.rebuilds(), 3);
}

#[test]
fn ten_thousand_block_projection_keeps_exact_source_rows() {
    use crate::tui::theme::Theme;

    let blocks: Vec<_> = (0..10_000)
        .map(|i| {
            if i % 20 == 0 {
                ScrollBlock::Tool {
                    call_id: Some(format!("call-{i}")),
                    name: "read_file".into(),
                    state: "done".into(),
                    subject: Some(format!("src/{i}.rs")),
                    detail: None,
                    parent_call_id: None,
                    cell_id: None,
                    kind: ActivityKind::Tool,
                    manual: false,
                    group_open: false,
                    wait: None,
                    started_at_ms: None,
                    finished_at_ms: None,
                    view: ItemView::default(),
                    mode: DisplayMode::Collapsed,
                }
            } else {
                ScrollBlock::Assistant(format!("response {i}"))
            }
        })
        .collect();
    let mut cache = TranscriptRenderCache::default();
    let render = cache.get(
        1,
        &blocks,
        &Theme::dark(),
        120,
        TranscriptChange::Structural,
        RenderPrefs::default(),
    );
    assert_eq!(render.lines.len(), render.layout.block_index.len());
    assert_eq!(render.lines.len(), render.layout.plain.len());
    assert!(render.layout.block_index.contains(&Some(0)));
    assert!(render.layout.block_index.contains(&Some(9_999)));
    assert_eq!(cache.rebuilds(), 1);
}

fn lines(block: &ScrollBlock, width: usize, prefs: RenderPrefs) -> Vec<String> {
    let theme = crate::tui::theme::Theme::dark();
    render::block_lines(block, &RowPlan::default(), &theme, width, prefs)
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// A process row is one item line whose label the cut shortens from the
/// right: a command longer than the row pushes the outcome and the
/// duration (the row's point) off the edge.
#[test]
fn a_long_process_command_keeps_its_outcome_and_duration_visible() {
    let command = "cargo nextest run --locked --profile offline --workspace --all-features \
                       -E 'package(plak) & test(review_)' --no-fail-fast --retries 0";
    let block = ScrollBlock::from_row(TranscriptRow::Process(ProcessNotice {
        job_id: "p261".into(),
        outcome: ProcessOutcome::Exited { code: Some(1) },
        duration_ms: Some(1_300_000),
        subject: Some(command.into()),
    }))
    .unwrap();
    for width in [80, 120] {
        let rows = lines(&block, width, RenderPrefs::default());
        let first = &rows[0];
        assert!(
            first.contains("exited 1") && first.contains("21m40s"),
            "width {width}: the outcome was cut off: {first:?}"
        );
    } // Narrower, the command is cut to the job id first, then the
    // duration drops; the outcome stays.
    for width in [20, 26, 32, 40] {
        let rows = lines(&block, width, RenderPrefs::default());
        let first = &rows[0];
        assert!(
            first.contains("exited 1"),
            "width {width}: the outcome was cut off: {first:?}"
        );
    }
    let first = &lines(&block, 32, RenderPrefs::default())[0];
    assert!(first.contains("p261"), "{first:?}");
}

/// A prompt with a send time wraps its first row short of the label;
/// every later row must read as it does without one. A hard newline
/// after the first row is not the soft-wrap space the band trims.
#[test]
fn a_send_time_keeps_the_second_line_indentation() {
    let prefs = RenderPrefs {
        prompt_clock: PromptClock::at(SENT_MS, 0),
        ..RenderPrefs::default()
    };
    let text = "fix this:\n    let x = 1;\n    let y = 2;";
    let with = lines(
        &ScrollBlock::User {
            text: text.into(),
            sent_at_ms: Some(SENT_MS),
        },
        60,
        prefs,
    );
    let without = lines(&ScrollBlock::user(text.into()), 60, prefs);
    assert!(with[0].trim_end().ends_with(" 14:32"), "{with:?}");
    assert_eq!(with[1..], without[1..]);
}

/// Every `ActivityKind::Agent` row
/// collapsed onto the `Agent` verb, so a `wait_agent` printed
/// `▶ ✓ Agent · 6.3s` and said nothing about what it waited on.
#[test]
fn the_read_and_mail_topology_tools_have_their_own_verbs() {
    for (name, verb, title) in [
        ("spawn_agent", ToolVerb::SpawnAgent, "Spawn Agent"),
        ("wait_agent", ToolVerb::WaitAgent, "Wait Agent"),
        ("list_agents", ToolVerb::ListAgents, "list"),
        ("send_agent_message", ToolVerb::MessageAgent, "message"),
    ] {
        assert_eq!(tool_verb(name, ActivityKind::Agent), verb);
        assert_eq!(display_tool_title_for(name, ActivityKind::Agent), title);
    }
    for (name, verb, title) in [
        (
            "interrupt_agent",
            ToolVerb::InterruptAgent,
            "Interrupt agent",
        ),
        ("close_agent", ToolVerb::CloseAgent, "Close agent"),
        ("recover_agent", ToolVerb::RecoverAgent, "Recover agent"),
        (
            "discard_workspace",
            ToolVerb::DiscardWorkspace,
            "Discard workspace",
        ),
        ("stop_all", ToolVerb::StopAll, "Stop all agents"),
        ("submit_result", ToolVerb::SubmitResult, "Submit result"),
    ] {
        assert_eq!(tool_verb(name, ActivityKind::Agent), verb);
        assert_eq!(display_tool_title_for(name, ActivityKind::Agent), title);
    }
}

/// `adopt_candidate` is the one topology call that
/// rewrites the human's checkout. Without an arm it fell through to
/// `ActivityKind::Agent` and the transcript said `Agent`, which is exactly
/// what the operator must not have to guess about.
#[test]
fn adopting_a_candidate_says_adopt_and_not_agent() {
    assert_eq!(
        tool_verb("adopt_candidate", ActivityKind::Agent),
        ToolVerb::Adopt
    );
    assert_eq!(
        display_tool_title_for("adopt_candidate", ActivityKind::Agent),
        "Adopt"
    );
    // The retired name is not resurrected as a synonym (AGENTS.md: no
    // compatibility shims).
    assert_eq!(
        tool_verb("merge_workspace", ActivityKind::Agent),
        ToolVerb::Agent
    );
}
