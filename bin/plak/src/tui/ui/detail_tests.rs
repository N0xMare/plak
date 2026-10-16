//! Transcript v2, part B: item detail, milestone and notice rows, and the
//! row interaction (spec 02 §5.4).

use std::sync::Arc;

use super::test_support::{
    key_ctrl_shift, left_mouse, restore_snapshot, say, serialize_frame, tool_item, tool_line,
    transcript_fixture,
};
use super::{App, DisplayMode};
use crate::tui::blocks::{RowTarget, ScrollBlock};
use crate::tui::presentation::ActivityKind;
use crate::tui::{HostEvent, UserAction, user_action_channel};
use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use plakous_host_protocol::views::transcript::{
    AgentMilestone, AutoContinue, CarriedFrom, MilestoneEvent, NoticeScope, ToolItem,
    TranscriptRow, WaitFacts,
};
use plakous_host_protocol::views::{CallDetail, CallDetailOutcome, CallDetailSource};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn paint(app: &mut App, width: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    serialize_frame(terminal.backend().buffer())
}

fn plain(app: &App) -> String {
    app.pane.transcript_cache.plain().join("\n")
}

fn golden(app: &mut App, story: &str, theme: &str, width: u16) {
    let actual = paint(app, width);
    let path = format!("tests/goldens/detail-{story}-{theme}-{width}x40.frame");
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(&path, actual).unwrap();
    } else {
        assert_eq!(
            actual,
            std::fs::read_to_string(&path).expect("detail golden"),
            "{path}"
        );
    }
}

fn canonical(call_id: &str, input: &str, output: &str) -> HostEvent {
    HostEvent::CallDetail(CallDetail {
        call_id: call_id.to_owned(),
        agent_id: None,
        session_id: "s1".to_owned(),
        outcome: CallDetailOutcome::Detail {
            input: input.to_owned(),
            output: output.to_owned(),
            input_truncated: false,
            output_truncated: false,
            source: CallDetailSource::Canonical,
            sources: Vec::new(),
        },
    })
}

/// A finished command the host answers for: fifty output lines.
fn command_app() -> App {
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    app.apply_host_event(tool_line(
        "run",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "done",
        Some("cargo test --workspace"),
    ));
    say(&mut app, "Done.");
    app
}

fn requests(app: &mut App) -> Vec<UserAction> {
    let (tx, rx) = user_action_channel();
    app.maybe_request_call_details(Some(&tx));
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|delivery| delivery.into_parts().0)
        .collect()
}

fn fifty_lines() -> String {
    (0..50)
        .map(|index| format!("line {index:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_opened_item_asks_once_and_shows_input_then_a_windowed_output() {
    let mut app = command_app();
    paint(&mut app, 120);
    app.toggle_tool_expand_at(0);
    assert_eq!(
        requests(&mut app),
        vec![UserAction::RequestCallDetail {
            call_id: "run".into(),
            agent_id: None,
            session_id: "s1".into(),
        }]
    );
    assert!(
        requests(&mut app).is_empty(),
        "an answer is awaited, not re-asked"
    );
    // The preview renders at once, unlabelled while the answer is pending.
    paint(&mut app, 120);
    assert!(
        plain(&app).contains("│ $ cargo test --workspace"),
        "{}",
        plain(&app)
    );
    assert!(!plain(&app).contains("preview"));

    app.apply_host_event(canonical(
        "run",
        r#"{"cmd":"cargo test --workspace\n  --all-features","workdir":"/w"}"#,
        &fifty_lines(),
    ));
    paint(&mut app, 120);
    let text = plain(&app);
    for wanted in [
        "▾",
        "│ $ cargo test --workspace",
        "│   --all-features",
        "│ workdir: /w",
        "│ ┄",
        "│ line 00",
        "│ line 19",
        "│ … 20 lines hidden ▸",
        "│ line 40",
        "│ line 49",
    ] {
        assert!(text.contains(wanted), "{wanted}: {text}");
    }
    assert!(!text.contains("line 25"), "{text}");
    assert!(!text.contains("{\""), "never raw JSON: {text}");

    // The hidden-lines row is clickable and shows the rest.
    let hidden = app
        .tool_hit_rows
        .iter()
        .zip(&app.tool_hit_targets)
        .find(|(_, target)| **target == RowTarget::HiddenLines)
        .map(|((_, y), _)| *y)
        .expect("the hidden-lines row is a hit row");
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        8,
        hidden,
    ));
    app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), 8, hidden));
    paint(&mut app, 120);
    assert!(plain(&app).contains("│ line 25"), "{}", plain(&app));
    assert!(!plain(&app).contains("lines hidden"));

    // Closing and reopening reads the cached answer.
    app.toggle_tool_expand_at(0);
    app.toggle_tool_expand_at(0);
    assert_eq!(
        requests(&mut app),
        [] as [plakous_host_protocol::SessionIntent<crate::surface_protocol::PlakExtension>; 0]
    );
    assert_eq!(app.call_details.waiting(), 0);
}

#[test]
fn canonical_answers_are_cached_per_session_and_survive_a_rebuild() {
    let mut app = command_app();
    app.toggle_tool_expand_at(0);
    let _ = requests(&mut app);
    app.apply_host_event(canonical("run", r#"{"cmd":"cargo test"}"#, "ok"));
    // The same session's transcript comes back from the host.
    app.apply_host_event(restore_snapshot(
        2,
        vec![TranscriptRow::Tool(ToolItem {
            subject: Some("cargo test --workspace".into()),
            ..tool_item("run", "exec_command")
        })],
    ));
    assert!(
        matches!(
            &app.pane.blocks[0],
            ScrollBlock::Tool {
                mode: DisplayMode::Expanded,
                ..
            }
        ),
        "a same-session snapshot keeps the open detail"
    );
    assert!(requests(&mut app).is_empty(), "the cache answers");
    paint(&mut app, 120);
    assert!(plain(&app).contains("│ ok"), "{}", plain(&app));

    // Another session asks again.
    app.session_id = Some("s2".into());
    app.toggle_tool_expand_at(0);
    app.toggle_tool_expand_at(0);
    if let Some(ScrollBlock::Tool { view, .. }) = app.pane.blocks.get_mut(0) {
        view.fetched = None;
    }
    app.request_item_detail(0);
    assert_eq!(requests(&mut app).len(), 1);
}

#[test]
fn a_preview_answer_and_an_unavailable_call_are_labelled_preview() {
    for outcome in [
        CallDetailOutcome::Detail {
            input: "cargo test --workspace".into(),
            output: "3 passed".into(),
            input_truncated: true,
            output_truncated: true,
            source: CallDetailSource::Preview,
            sources: Vec::new(),
        },
        CallDetailOutcome::Unavailable {
            reason: "not in history".into(),
        },
    ] {
        let mut app = command_app();
        if let Some(ScrollBlock::Tool { detail, .. }) = app.pane.blocks.get_mut(0) {
            *detail = Some("3 passed".into());
        }
        app.toggle_tool_expand_at(0);
        let _ = requests(&mut app);
        app.apply_host_event(HostEvent::CallDetail(CallDetail {
            call_id: "run".into(),
            agent_id: None,
            session_id: "s1".into(),
            outcome: outcome.clone(),
        }));
        paint(&mut app, 120);
        let text = plain(&app);
        assert!(text.contains("│ preview"), "{outcome:?}: {text}");
        assert!(text.contains("│ $ cargo test --workspace"), "{text}");
        assert!(text.contains("│ 3 passed"), "{text}");
        assert!(
            !text.contains("truncated"),
            "a preview says preview: {text}"
        );
    }
}

#[test]
fn a_dropped_answer_is_asked_again_while_the_item_stays_open() {
    let mut app = command_app();
    app.toggle_tool_expand_at(0);
    assert_eq!(requests(&mut app).len(), 1);
    app.call_details.age_asks(std::time::Duration::from_secs(6));
    assert_eq!(requests(&mut app).len(), 1, "re-asked after the window");
    app.toggle_tool_expand_at(0);
    assert!(
        requests(&mut app).is_empty(),
        "a closed item is not re-asked"
    );
    assert_eq!(app.call_details.waiting(), 0);
}

#[test]
fn edits_render_as_a_diff_and_other_tools_as_key_values() {
    let mut app = transcript_fixture();
    app.apply_host_event(tool_line(
        "edit",
        None,
        None,
        ActivityKind::Tool,
        "edit_file",
        "done",
        Some("src/a.rs +1/-1"),
    ));
    say(&mut app, "x");
    app.toggle_tool_expand_at(0);
    let _ = requests(&mut app);
    app.apply_host_event(canonical(
        "edit",
        r#"{"file_path":"src/a.rs","old_string":"one\ntwo\nthree","new_string":"one\n2\nthree"}"#,
        "applied",
    ));
    paint(&mut app, 120);
    let text = plain(&app);
    for wanted in [
        "│ --- src/a.rs",
        "│ @@ -1,3 +1,3 @@",
        "│  one",
        "│ -two",
        "│ +2",
        "│  three",
    ] {
        assert!(text.contains(wanted), "{wanted}: {text}");
    }

    let mut app = transcript_fixture();
    app.apply_host_event(tool_line(
        "fetch",
        None,
        None,
        ActivityKind::Tool,
        "custom_lookup",
        "done",
        Some("query"),
    ));
    say(&mut app, "x");
    app.toggle_tool_expand_at(0);
    let _ = requests(&mut app);
    app.apply_host_event(canonical(
        "fetch",
        r#"{"query":"needle","limit":5,"scope":{"paths":["a","b"]},"note":"one\ntwo"}"#,
        "",
    ));
    paint(&mut app, 120);
    let text = plain(&app);
    for wanted in [
        "│ query: needle",
        "│ limit: 5",
        "│ scope:",
        "│   paths: a, b",
        "│ note:",
        "│   one",
    ] {
        assert!(text.contains(wanted), "{wanted}: {text}");
    }
    assert!(!text.contains('{'), "{text}");
    assert!(!text.contains('┄'), "no output, no separator: {text}");
}

fn wait_row(state: &str) -> (HostEvent, TranscriptRow) {
    let facts = WaitFacts {
        timeout_ms: Some(3_600_000),
        woke_by: Some("c39 finished".into()),
        waited_ms: Some(720_000),
        settled_agent: Some("c39".into()),
    };
    let HostEvent::ToolLine {
        call_id,
        parent_call_id,
        cell_id,
        kind,
        name,
        state,
        subject,
        detail,
        duration_ms,
        ..
    } = tool_line(
        "wait",
        None,
        None,
        ActivityKind::Agent,
        "wait_agent",
        state,
        Some("c39 · up to 3600s"),
    )
    else {
        unreachable!()
    };
    let live = HostEvent::ToolLine {
        call_id,
        parent_call_id,
        cell_id,
        kind,
        name,
        state: state.clone(),
        subject: subject.clone(),
        detail,
        duration_ms,
        started_at_ms: None,
        finished_at_ms: None,
        wait: Some(facts.clone()),
    };
    let row = TranscriptRow::Tool(ToolItem {
        state,
        subject,
        wait: Some(facts),
        ..tool_item("wait", "wait_agent")
    });
    (live, row)
}

fn story_rows() -> Vec<TranscriptRow> {
    vec![
        TranscriptRow::SessionHeader {
            session_id: "0a1b2c3d-0000".into(),
            carried_from: Some(CarriedFrom {
                session_id: "4e5f6a7b-0000".into(),
                state_key: "r21.e19.p27.t2".into(),
            }),
        },
        TranscriptRow::user("fan out"),
        TranscriptRow::Milestone(AgentMilestone {
            agent_id: "c23".into(),
            event: MilestoneEvent::Spawned,
            template: Some("worker".into()),
            text: Some("Port the renderer\nthen run the goldens\nand report back".into()),
            duration_ms: None,
            candidate: None,
            files: None,
            paths: Vec::new(),
        }),
        TranscriptRow::Milestone(AgentMilestone {
            agent_id: "c24".into(),
            event: MilestoneEvent::Failed,
            template: None,
            text: Some("provider unreachable".into()),
            duration_ms: None,
            candidate: None,
            files: None,
            paths: Vec::new(),
        }),
        TranscriptRow::steer("also check light themes"),
        TranscriptRow::AutoContinue(AutoContinue {
            agents_finished: 2,
            processes_exited: 1,
            agents: vec!["c23".into(), "c25".into()],
        }),
        TranscriptRow::Milestone(AgentMilestone {
            agent_id: "c23".into(),
            event: MilestoneEvent::Finished,
            template: None,
            text: Some("All goldens pass.".into()),
            duration_ms: Some(840_000),
            candidate: None,
            files: None,
            paths: Vec::new(),
        }),
        TranscriptRow::Milestone(AgentMilestone {
            agent_id: "c23".into(),
            event: MilestoneEvent::Adopted,
            template: None,
            text: None,
            duration_ms: None,
            candidate: Some("cand-c23-a1".into()),
            files: Some(16),
            paths: Vec::new(),
        }),
        TranscriptRow::error(NoticeScope::Turn, "turn failed: provider unreachable"),
        TranscriptRow::Interrupted,
    ]
}

#[test]
fn rows_render_alike_live_and_restored_in_both_themes_and_widths() {
    for (theme_name, theme) in [
        ("dark", crate::tui::Theme::dark()),
        ("light", crate::tui::Theme::light()),
    ] {
        for width in [120, 40] {
            let mut live = transcript_fixture();
            live.theme = theme.clone();
            let mut rows = story_rows();
            let (wait_live, wait_row) = wait_row("done");
            let interrupted = rows.pop().unwrap();
            let error = rows.pop().unwrap();
            for row in rows.clone() {
                live.apply_host_event(HostEvent::Row(row));
            }
            live.apply_host_event(wait_live);
            live.apply_host_event(HostEvent::Row(error.clone()));
            live.apply_host_event(HostEvent::Row(interrupted.clone()));
            rows.extend([wait_row, error, interrupted]);
            let mut restored = transcript_fixture();
            restored.theme = theme.clone();
            restored.apply_host_event(restore_snapshot(1, rows));
            live.pane.tool_started.clear();
            let frame = paint(&mut live, width);
            assert_eq!(frame, paint(&mut restored, width));
            let text = plain(&live);
            for wanted in [
                "carried from 4e5f6a7b (r21.e19.p27.t2) · session 0a1b2c3d",
                "◆ spawned c23 · worker · Port the renderer",
                "✗ c24 failed · provider unreachable",
                "✓ c23 finished · 14m",
                "⇲ adopted cand-c23-a1 · 16 files",
                "⊘ interrupted",
            ] {
                if width == 120 {
                    assert!(text.contains(wanted), "{wanted}: {text}");
                }
            }
            // The story's stored auto-continue row shows neither live nor
            // restored.
            assert!(!text.contains("auto-continue"), "{text}");
            if width == 120 {
                assert!(
                    text.contains("Waited for agents · up to 60m · woke after 12m (c39 finished)"),
                    "{text}"
                );
                assert!(frame.contains("steer"), "{frame}");
            }
            golden(&mut live, "rows", theme_name, width);

            // A milestone with text opens to all of it, live and restored.
            let spawned = live
                .pane
                .blocks
                .iter()
                .position(|block| matches!(block, ScrollBlock::Milestone { .. }))
                .unwrap();
            live.toggle_tool_expand_at(spawned);
            restored.toggle_tool_expand_at(spawned);
            assert_eq!(paint(&mut live, width), paint(&mut restored, width));
            assert!(
                plain(&live).contains("│ and report back"),
                "{}",
                plain(&live)
            );
            golden(&mut live, "milestone-open", theme_name, width);
        }
    }
}

/// An adoption opens like every other milestone: its candidate, the child
/// and its template, the landed file count, then any landed paths the host
/// recorded. A row without text still opens to its facts; live and restored
/// paint alike.
#[test]
fn an_adoption_opens_to_its_facts_live_and_restored() {
    let adopted = |template: Option<&str>, text: Option<&str>| {
        TranscriptRow::Milestone(AgentMilestone {
            agent_id: "c23".into(),
            event: MilestoneEvent::Adopted,
            template: template.map(str::to_owned),
            text: text.map(str::to_owned),
            duration_ms: None,
            candidate: Some("cand-c23-a1".into()),
            files: Some(2),
            paths: Vec::new(),
        })
    };
    for (row, wanted) in [
        (
            adopted(Some("worker"), Some("src/a.rs\nsrc/b.rs")),
            &[
                "│ candidate cand-c23-a1",
                "│ agent c23 · worker",
                "│ 2 files landed",
                "│ src/a.rs",
                "│ src/b.rs",
            ][..],
        ),
        (
            adopted(None, None),
            &["│ candidate cand-c23-a1", "│ agent c23", "│ 2 files landed"][..],
        ),
    ] {
        let mut live = transcript_fixture();
        live.apply_host_event(HostEvent::Row(row.clone()));
        let mut restored = transcript_fixture();
        restored.apply_host_event(restore_snapshot(1, vec![row]));
        for app in [&mut live, &mut restored] {
            let index = app
                .pane
                .blocks
                .iter()
                .position(|block| matches!(block, ScrollBlock::Milestone { .. }))
                .unwrap();
            assert_eq!(
                app.pane.blocks[index].display_mode(),
                Some(DisplayMode::Collapsed)
            );
            app.toggle_tool_expand_at(index);
        }
        assert_eq!(paint(&mut live, 120), paint(&mut restored, 120));
        let text = plain(&live);
        assert!(text.contains("⇲ adopted cand-c23-a1 · 2 files"), "{text}");
        for line in wanted {
            assert!(text.contains(line), "{line}: {text}");
        }
    }
}

#[test]
fn a_running_wait_shows_its_ceiling_and_a_failed_wait_its_error() {
    let mut app = transcript_fixture();
    let (running, _) = wait_row("running");
    app.apply_host_event(running);
    paint(&mut app, 120);
    assert!(
        plain(&app).contains("Waiting for agents · up to 60m"),
        "{}",
        plain(&app)
    );
    assert!(!plain(&app).contains("woke"));
    let (failed, _) = wait_row("failed:no such agent");
    app.apply_host_event(failed);
    paint(&mut app, 120);
    assert!(
        plain(&app).contains(
            "Waited for agents · up to 60m · woke after 12m (c39 finished) · no such agent"
        ),
        "{}",
        plain(&app)
    );
}

#[test]
fn item_detail_goldens_cover_both_themes_and_widths() {
    for (theme_name, theme) in [
        ("dark", crate::tui::Theme::dark()),
        ("light", crate::tui::Theme::light()),
    ] {
        for width in [120, 40] {
            let mut app = command_app();
            app.theme = theme.clone();
            app.toggle_tool_expand_at(0);
            let _ = requests(&mut app);
            app.apply_host_event(canonical(
                "run",
                r#"{"cmd":"cargo test --workspace --all-features -- --nocapture 'detail window'"}"#,
                &fifty_lines(),
            ));
            // Scrolled up so the item, its input and the hidden-lines row
            // are all on screen.
            paint(&mut app, width);
            app.pane.scroll_from_bottom = 12;
            golden(&mut app, "command-open", theme_name, width);
        }
    }
}

#[test]
fn a_click_toggles_the_row_under_it_and_hover_lights_only_that_row() {
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    for (id, subject) in [("a", "cargo build"), ("b", "cargo test")] {
        app.apply_host_event(tool_line(
            id,
            None,
            None,
            ActivityKind::Process,
            "exec_command",
            "done",
            Some(subject),
        ));
    }
    say(&mut app, "Done.");
    paint(&mut app, 120);
    // One summary row; a click opens the group.
    assert_eq!(app.tool_hit_targets, vec![RowTarget::Group]);
    let (_, y) = app.tool_hit_rows[0];
    app.handle_mouse(left_mouse(MouseEventKind::Down(MouseButton::Left), 4, y));
    app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), 4, y));
    paint(&mut app, 120);
    assert_eq!(
        app.tool_hit_targets,
        vec![RowTarget::Group, RowTarget::Item, RowTarget::Item]
    );
    // A click on the second item opens that item only.
    let (block, y) = app.tool_hit_rows[2];
    app.handle_mouse(left_mouse(MouseEventKind::Down(MouseButton::Left), 4, y));
    app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), 4, y));
    assert_eq!(
        app.pane.blocks[block].display_mode(),
        Some(DisplayMode::Expanded)
    );
    assert_eq!(
        app.pane.blocks[0].display_mode(),
        Some(DisplayMode::Collapsed)
    );
    let _ = requests(&mut app);

    app.handle_mouse(left_mouse(MouseEventKind::Moved, 4, 39));
    let buffer = |app: &mut App| {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    };
    let cold = buffer(&mut app);
    app.handle_mouse(left_mouse(MouseEventKind::Moved, 4, y));
    let hot = buffer(&mut app);
    let changed: Vec<u16> = (0..40)
        .filter(|row| (0..120).any(|column| cold[(column, *row)] != hot[(column, *row)]))
        .collect();
    assert_eq!(changed, vec![y], "only the hovered row lights");

    // Ctrl+Shift+O folds everything that is open.
    app.handle_key(key_ctrl_shift(KeyCode::Char('o')));
    assert!(app.pane.blocks.iter().all(|block| !matches!(
        block,
        ScrollBlock::Tool {
            group_open: true,
            ..
        } | ScrollBlock::Tool {
            mode: DisplayMode::Expanded,
            ..
        }
    )));
}

#[test]
fn copying_a_cut_row_copies_its_full_text() {
    let command = "cargo test --workspace --all-features -- --nocapture detail_window_names";
    let mut app = transcript_fixture();
    app.apply_host_event(tool_line(
        "run",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "failed:exit 2",
        Some(command),
    ));
    say(&mut app, "Done.");
    paint(&mut app, 40);
    let text = plain(&app);
    assert!(text.contains('…'), "the row is cut at 40 columns: {text}");
    let body = app.transcript_body_area.unwrap();
    let (_, y) = app.tool_hit_rows[0];
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.right() - 1,
        y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.right() - 1,
        y,
    ));
    assert!(app.copy_selection_after_draw);
    assert_eq!(
        app.pane.blocks[0].display_mode(),
        Some(DisplayMode::Collapsed),
        "a drag selects, it does not toggle"
    );
    paint(&mut app, 40);
    let copied = app.take_transcript_selection_text().unwrap();
    assert_eq!(copied, format!("  ✗ $ {command} · exit 2"));
}

#[test]
fn copying_a_prompt_or_steer_row_leaves_out_the_time_and_the_steer_tag() {
    let sent = 1_791_383_520_000;
    let mut app = transcript_fixture();
    // The pane paints send times in the process zone, not a fixed offset.
    let label = crate::tui::blocks::PromptClock::in_zone(app.time_zone, app.unix_now_ms())
        .label(sent)
        .unwrap();
    let clock = label.rsplit(' ').next().unwrap();
    app.apply_host_event(HostEvent::Row(TranscriptRow::User {
        text: "ship it\n  keep indent".into(),
        sent_at_ms: Some(sent),
    }));
    app.apply_host_event(HostEvent::Row(TranscriptRow::Steer {
        text: "also docs".into(),
        sent_at_ms: Some(sent),
    }));
    let frame = paint(&mut app, 80);
    assert!(frame.contains(clock), "{label} not painted:\n{frame}");
    assert!(frame.contains("steer"), "{frame}");
    let body = app.transcript_body_area.unwrap();
    let last = u16::try_from(app.pane.transcript_cache.plain().len() - 1).unwrap();
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        body.y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.right() - 1,
        body.y + last,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.right() - 1,
        body.y + last,
    ));
    paint(&mut app, 80);
    let copied = app.take_transcript_selection_text().unwrap();
    assert!(!copied.contains(clock), "{copied}");
    assert!(!copied.contains("steer"), "{copied}");
    assert!(copied.contains("ship it"), "{copied}");
    assert!(copied.contains("  keep indent"), "{copied}");
    assert!(copied.contains("also docs"), "{copied}");
}

#[test]
fn opening_a_merged_row_lists_each_call_and_each_opens_on_its_own() {
    let mut app = transcript_fixture();
    app.render_prefs.group_tool_calls = false;
    for (id, path) in [("r1", "a.rs"), ("r2", "b.rs"), ("r3", "c.rs")] {
        app.apply_host_event(tool_line(
            id,
            None,
            None,
            ActivityKind::Tool,
            "read_file",
            "done",
            Some(path),
        ));
    }
    say(&mut app, "x");
    paint(&mut app, 120);
    let merged = |app: &App, caret: char| {
        app.pane
            .transcript_cache
            .plain()
            .iter()
            .any(|row| row.contains("✓ Explored 3 files") && row.ends_with(caret))
    };
    assert!(merged(&app, '▸'), "{}", plain(&app));
    assert_eq!(app.tool_hit_targets, vec![RowTarget::Merge]);
    app.toggle_tool_expand_at(0);
    paint(&mut app, 120);
    let text = plain(&app);
    assert!(merged(&app, '▾'), "{text}");
    for wanted in ["├─  ✓ Read a.rs", "├─  ✓ Read b.rs", "└─  ✓ Read c.rs"] {
        assert!(text.contains(wanted), "{wanted}: {text}");
    }
    assert_eq!(
        app.tool_hit_targets,
        vec![
            RowTarget::Merge,
            RowTarget::Item,
            RowTarget::Item,
            RowTarget::Item
        ]
    );
    app.toggle_row(1, RowTarget::Item);
    assert_eq!(
        app.pane.blocks[1].display_mode(),
        Some(DisplayMode::Expanded)
    );
    assert_eq!(requests(&mut app).len(), 1);
}

#[test]
fn fetched_detail_is_shared_by_reference() {
    let mut app = command_app();
    app.toggle_tool_expand_at(0);
    let _ = requests(&mut app);
    app.apply_host_event(canonical("run", "{}", "ok"));
    let Some(ScrollBlock::Tool {
        view:
            crate::tui::blocks::ItemView {
                fetched: Some(fetched),
                ..
            },
        ..
    }) = app.pane.blocks.first()
    else {
        panic!("fetched")
    };
    assert!(Arc::strong_count(fetched) >= 2, "cached and attached");
}

#[test]
fn every_built_in_theme_defines_readable_detail_tokens() {
    use crate::tui::theme::{Theme, ThemeId};
    for id in ThemeId::ALL {
        let theme = Theme::for_id(id);
        for (name, style) in [
            ("detail_output", theme.detail_output),
            ("diff_added", theme.diff_added),
            ("diff_removed", theme.diff_removed),
            ("diff_hunk", theme.diff_hunk),
        ] {
            let fg = style
                .fg
                .unwrap_or_else(|| panic!("{id:?} {name} has a colour"));
            assert_ne!(
                fg, theme.app_bg,
                "{id:?} {name} is visible on the background"
            );
        }
        assert_ne!(theme.diff_added.fg, theme.diff_removed.fg, "{id:?}");
    }
}

#[test]
fn an_answer_for_another_session_is_dropped() {
    let mut app = command_app();
    app.toggle_tool_expand_at(0);
    let _ = requests(&mut app);
    app.apply_host_event(HostEvent::CallDetail(CallDetail {
        call_id: "run".into(),
        agent_id: None,
        session_id: "s0".into(),
        outcome: CallDetailOutcome::Detail {
            input: "{}".into(),
            output: "OTHER SESSION".into(),
            input_truncated: false,
            output_truncated: false,
            source: CallDetailSource::Canonical,
            sources: Vec::new(),
        },
    }));
    paint(&mut app, 120);
    assert!(!plain(&app).contains("OTHER SESSION"), "{}", plain(&app));
    assert_eq!(
        app.call_details.waiting(),
        1,
        "still awaiting its own answer"
    );
    // A session change drops the request the old session asked.
    app.session_id = Some("s2".into());
    let _ = requests(&mut app);
    assert_eq!(app.call_details.waiting(), 0);
}

#[test]
fn an_adoption_counts_its_files_in_the_singular_and_plural() {
    for (files, label) in [
        (1, "⇲ adopted cand-c1-a1 · 1 file"),
        (16, "⇲ adopted cand-c1-a1 · 16 files"),
    ] {
        let block = ScrollBlock::from_row(TranscriptRow::Milestone(AgentMilestone {
            agent_id: "c1".into(),
            event: MilestoneEvent::Adopted,
            template: None,
            text: None,
            duration_ms: None,
            candidate: Some("cand-c1-a1".into()),
            files: Some(files),
            paths: Vec::new(),
        }))
        .unwrap();
        assert_eq!(block.as_plain_line(), label);
    }
}

/// A steer whose first line fills the band keeps its `steer` tag
/// ("the user band with a small `steer` tag").
#[test]
fn a_long_steer_keeps_its_steer_tag() {
    let mut app = transcript_fixture();
    app.apply_host_event(HostEvent::Row(TranscriptRow::steer(
        "word ".repeat(60).trim_end().to_owned(),
    )));
    for width in [120, 40] {
        let frame = paint(&mut app, width);
        assert!(frame.contains("steer"), "{frame}");
        // The copy projection omits the tag; the painted row keeps it.
        let symbols = frame
            .lines()
            .find(|line| line.contains("symbols") && line.contains('❯'))
            .unwrap();
        let painted = symbols.trim_end_matches(['·', ' ']);
        assert!(painted.ends_with("steer"), "{symbols}");
    }
}

/// The open item's `▾` stays visible when the transcript scrolls.
#[test]
fn the_top_rows_caret_stays_visible_when_scrolled() {
    let mut app = command_app();
    paint(&mut app, 120);
    app.toggle_tool_expand_at(0);
    let _ = requests(&mut app);
    app.apply_host_event(canonical("run", "cargo test --workspace", &fifty_lines()));
    paint(&mut app, 120);
    app.pane.scroll_from_bottom = 12;
    let frame = paint(&mut app, 120);
    assert!(plain(&app).lines().next().unwrap().contains('▾'));
    let top = frame
        .lines()
        .find(|line| line.starts_with("00 symbols"))
        .unwrap()
        .to_owned();
    assert!(top.contains('▾'), "{top}");
}

/// A `preview` answer (the call not yet committed, or a `busy` refusal)
/// is asked again once the item is closed and reopened.
#[test]
fn a_busy_answer_is_asked_again_on_reopen() {
    let mut app = command_app();
    app.toggle_tool_expand_at(0);
    assert_eq!(requests(&mut app).len(), 1);
    app.apply_host_event(HostEvent::CallDetail(CallDetail {
        call_id: "run".into(),
        agent_id: None,
        session_id: "s1".into(),
        outcome: CallDetailOutcome::Unavailable {
            reason: "busy".into(),
        },
    }));
    app.toggle_tool_expand_at(0);
    app.toggle_tool_expand_at(0);
    assert_eq!(
        requests(&mut app).len(),
        1,
        "a transient answer is not final for the row"
    );
}

/// `✓ c23 finished ▸` opens to the submitted result, pretty.
#[test]
fn a_finished_childs_json_result_opens_pretty() {
    let mut app = transcript_fixture();
    app.apply_host_event(HostEvent::Row(TranscriptRow::Milestone(AgentMilestone {
        agent_id: "c23".into(),
        event: MilestoneEvent::Finished,
        template: None,
        text: Some(r#"{"verdict":"ok","findings":[{"severity":"low"}]}"#.into()),
        duration_ms: Some(840_000),
        candidate: None,
        files: None,
        paths: Vec::new(),
    })));
    let idx = app
        .pane
        .blocks
        .iter()
        .position(|block| matches!(block, ScrollBlock::Milestone { .. }))
        .unwrap();
    app.toggle_tool_expand_at(idx);
    paint(&mut app, 120);
    let text = plain(&app);
    for row in [
        "    │ {",
        "    │   \"verdict\": \"ok\",",
        "    │   \"findings\": [",
        "    │       \"severity\": \"low\"",
        "    │ }",
    ] {
        assert!(
            text.lines().any(|line| line.trim_end() == row),
            "{row}: {text}"
        );
    }
}

fn preview(call_id: &str) -> HostEvent {
    HostEvent::CallDetail(CallDetail {
        call_id: call_id.to_owned(),
        agent_id: None,
        session_id: "s1".to_owned(),
        outcome: CallDetailOutcome::Detail {
            input: "cargo test --workspace".into(),
            output: "3 passed".into(),
            input_truncated: true,
            output_truncated: true,
            source: CallDetailSource::Preview,
            sources: Vec::new(),
        },
    })
}

/// `busy` is the host's read queue, not an answer about the call: the row
/// keeps its unlabelled preview and the call is asked again while open.
#[test]
fn a_busy_refusal_is_no_answer_and_is_asked_again_while_open() {
    let mut app = command_app();
    app.toggle_tool_expand_at(0);
    assert_eq!(requests(&mut app).len(), 1);
    app.apply_host_event(HostEvent::CallDetail(CallDetail {
        call_id: "run".into(),
        agent_id: None,
        session_id: "s1".into(),
        outcome: CallDetailOutcome::Unavailable {
            reason: "busy".into(),
        },
    }));
    paint(&mut app, 120);
    assert!(!plain(&app).contains("preview"), "{}", plain(&app));
    assert!(
        requests(&mut app).is_empty(),
        "not before the re-ask window"
    );
    app.call_details.age_asks(std::time::Duration::from_secs(6));
    assert_eq!(requests(&mut app).len(), 1);
    app.apply_host_event(canonical("run", "{}", "3 passed"));
    paint(&mut app, 120);
    assert!(plain(&app).contains("│ 3 passed"), "{}", plain(&app));
}

/// A call that ended moments ago may answer `preview` only because its
/// canonical history is not committed yet: it is asked once more while open,
/// and the canonical answer replaces the preview.
#[test]
fn a_preview_for_a_call_that_just_ended_is_asked_once_more() {
    let mut app = command_app();
    app.toggle_tool_expand_at(0);
    assert_eq!(requests(&mut app).len(), 1);
    app.apply_host_event(preview("run"));
    paint(&mut app, 120);
    assert!(plain(&app).contains("│ preview"), "{}", plain(&app));
    assert_eq!(
        requests(&mut app),
        [] as [plakous_host_protocol::SessionIntent<crate::surface_protocol::PlakExtension>; 0]
    );
    app.call_details.age_asks(std::time::Duration::from_secs(6));
    assert_eq!(requests(&mut app).len(), 1, "asked once more");
    app.apply_host_event(preview("run"));
    app.call_details.age_asks(std::time::Duration::from_secs(6));
    assert!(requests(&mut app).is_empty(), "only once");
    assert_eq!(app.call_details.waiting(), 0);

    // Closing drops the preview; reopening asks again and takes canonical.
    app.toggle_tool_expand_at(0);
    app.toggle_tool_expand_at(0);
    assert_eq!(requests(&mut app).len(), 1);
    app.apply_host_event(canonical("run", "{}", "3 passed"));
    paint(&mut app, 120);
    assert!(!plain(&app).contains("preview"), "{}", plain(&app));
}

/// A restored call ended long before this surface saw it: its preview is
/// the answer while the item is open, not a race to retry.
#[test]
fn a_preview_for_a_restored_call_is_not_asked_again_while_open() {
    let mut app = transcript_fixture();
    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::Tool(tool_item("run", "exec_command"))],
    ));
    app.toggle_tool_expand_at(0);
    assert_eq!(requests(&mut app).len(), 1);
    app.apply_host_event(preview("run"));
    app.call_details.age_asks(std::time::Duration::from_secs(6));
    assert_eq!(
        requests(&mut app),
        [] as [plakous_host_protocol::SessionIntent<crate::surface_protocol::PlakExtension>; 0]
    );
    assert_eq!(app.call_details.waiting(), 0);
}

/// A steer longer than the band keeps its tag on the first row at both
/// widths, live and restored alike.
#[test]
fn a_long_steer_golden_covers_both_themes_and_widths() {
    let text = "keep the tag visible: ".repeat(12).trim_end().to_owned();
    for (theme_name, theme) in [
        ("dark", crate::tui::Theme::dark()),
        ("light", crate::tui::Theme::light()),
    ] {
        for width in [120, 40] {
            let mut live = transcript_fixture();
            live.theme = theme.clone();
            live.apply_host_event(HostEvent::Row(TranscriptRow::steer(text.clone())));
            let mut restored = transcript_fixture();
            restored.theme = theme.clone();
            restored.apply_host_event(restore_snapshot(
                1,
                vec![TranscriptRow::steer(text.clone())],
            ));
            assert_eq!(paint(&mut live, width), paint(&mut restored, width));
            golden(&mut live, "steer-long", theme_name, width);
        }
    }
}

/// Output text that reads like a hidden-lines row or a spinner is output:
/// only the real hidden-lines row is clickable.
#[test]
fn output_that_looks_like_chrome_is_not_clickable() {
    let mut app = command_app();
    app.toggle_tool_expand_at(0);
    let _ = requests(&mut app);
    let output = format!("… 7 lines hidden ▸\n  ⠼ not a tip\n{}", fifty_lines());
    app.apply_host_event(canonical("run", "{}", &output));
    paint(&mut app, 120);
    app.pane.scroll_from_bottom = 1_000;
    paint(&mut app, 120);
    let text = plain(&app);
    assert!(text.contains("│ … 7 lines hidden ▸"), "{text}");
    assert!(text.contains("│ … 22 lines hidden ▸"), "{text}");
    let hidden = app
        .tool_hit_targets
        .iter()
        .filter(|target| **target == RowTarget::HiddenLines)
        .count();
    assert_eq!(hidden, 1, "{:?}", app.tool_hit_targets);
}

/// Focus is one background highlight on the row, whatever its type; a
/// thinking row keeps its `◆`.
#[test]
fn the_focused_row_is_highlighted_and_keeps_its_glyph() {
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "r1".into(),
        text: "weighing options".into(),
    });
    app.apply_host_event(HostEvent::ReasoningSealed {
        location_key: "r1".into(),
    });
    say(&mut app, "Done.");
    paint(&mut app, 120);
    let idx = app
        .pane
        .blocks
        .iter()
        .position(|block| matches!(block, ScrollBlock::Thinking { .. }))
        .unwrap();
    app.toggle_tool_expand_at(idx);
    app.toggle_tool_expand_at(idx);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let body = app.transcript_body_area.unwrap();
    let buffer = terminal.backend().buffer();
    let row = (body.y..body.bottom())
        .find(|&y| buffer[(body.x + 2, y)].symbol() == "◆")
        .expect("the thought keeps ◆");
    assert_eq!(
        buffer[(body.x + 10, row)].bg,
        app.theme.row_focus.bg.unwrap()
    );
    assert!(
        (body.y..body.bottom())
            .filter(|&y| y != row)
            .all(|y| buffer[(body.x + 10, y)].bg != app.theme.row_focus.bg.unwrap())
    );
}

/// A selection that starts inside a row copies what it covers; a whole cut
/// row copies its command with its own line breaks.
#[test]
fn copy_substitutes_only_whole_rows_and_keeps_real_newlines() {
    let command = "cargo test --workspace\n  --all-features -- --nocapture detail_window_names";
    let mut app = transcript_fixture();
    app.apply_host_event(tool_line(
        "run",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "done",
        Some(command),
    ));
    say(&mut app, "Done.");
    paint(&mut app, 40);
    let body = app.transcript_body_area.unwrap();
    let (_, y) = app.tool_hit_rows[0];
    let select = |app: &mut App, from: u16| {
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), from),
            (MouseEventKind::Drag(MouseButton::Left), body.right() - 1),
            (MouseEventKind::Up(MouseButton::Left), body.right() - 1),
        ] {
            app.handle_mouse(left_mouse(kind, column, y));
        }
        paint(app, 40);
        app.take_transcript_selection_text().unwrap()
    };
    assert_eq!(select(&mut app, body.x), format!("  ✓ $ {command}"));
    let part = select(&mut app, body.x + 6);
    assert!(part.starts_with("cargo test"), "{part:?}");
    assert!(part.contains('…') && !part.contains('\n'), "{part:?}");
}

#[test]
fn cell_outline_source_output_and_nested_details_toggle_independently() {
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    super::test_support::compose_cell(
        &mut app,
        "cell",
        "exec_js",
        "done:4.2s",
        "1 tools",
        "const result = await tools.exec_command({cmd: 'pwd'});",
        &[("exec_command", "done", "pwd", "CHILD PREVIEW")],
    );
    say(&mut app, "Done.");
    paint(&mut app, 80);
    assert!(plain(&app).contains("$ pwd"));
    assert!(!plain(&app).contains("const result"));
    app.toggle_tool_expand_at(0);
    let _ = requests(&mut app);
    app.apply_host_event(canonical("cell", "{}", "CELL OUTPUT"));
    paint(&mut app, 80);
    let rows = plain(&app);
    assert!(rows.find("$ pwd").unwrap() < rows.find("const result").unwrap());
    assert!(rows.contains("output ▸") && !rows.contains("CELL OUTPUT"));
    assert_eq!(
        app.pane.blocks[1].display_mode(),
        Some(DisplayMode::Collapsed)
    );
    let hit = app
        .tool_hit_targets
        .iter()
        .position(|t| *t == RowTarget::CellOutput(0))
        .unwrap();
    let (block, y) = app.tool_hit_rows[hit];
    let body = app.transcript_body_area.unwrap();
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x + 8,
        y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.x + 8,
        y,
    ));
    paint(&mut app, 80);
    assert_eq!(block, 1, "footer keeps the outline tail's block identity");
    assert!(plain(&app).contains("output ▾") && plain(&app).contains("CELL OUTPUT"));
    app.toggle_tool_expand_at(1);
    paint(&mut app, 80);
    assert!(plain(&app).contains("CHILD PREVIEW"));
    app.toggle_tool_expand_at(0);
    paint(&mut app, 80);
    assert!(!plain(&app).contains("CELL OUTPUT") && !plain(&app).contains("const result"));
    assert!(
        plain(&app).contains("CHILD PREVIEW"),
        "closing the cell does not close its rows"
    );
}

#[test]
fn cell_and_nested_output_windows_have_distinct_hidden_line_targets() {
    let mut app = transcript_fixture();
    super::test_support::compose_cell(
        &mut app,
        "cell",
        "exec_py",
        "done",
        "1 tools",
        "print('ok')",
        &[("exec_command", "done", "pwd", "preview")],
    );
    say(&mut app, "Done.");
    app.toggle_tool_expand_at(0);
    app.toggle_tool_expand_at(1);
    let _ = requests(&mut app);
    let output = (0..40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.apply_host_event(canonical("cell", "{}", &output));
    app.apply_host_event(canonical("cell-child-0", "{}", &output));
    app.toggle_row(1, RowTarget::CellOutput(0));
    let mut terminal = Terminal::new(TestBackend::new(100, 100)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(app.tool_hit_targets.contains(&RowTarget::HiddenLines));
    assert!(
        app.tool_hit_targets
            .contains(&RowTarget::CellHiddenLines(0))
    );
    app.toggle_row(1, RowTarget::CellHiddenLines(0));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(app.tool_hit_targets.contains(&RowTarget::HiddenLines));
    assert!(
        !app.tool_hit_targets
            .contains(&RowTarget::CellHiddenLines(0))
    );
}

#[test]
fn a_one_line_thought_copies_its_full_clean_summary_without_toggling() {
    let mut app = transcript_fixture();
    let summary = "A very long clean thought summary with more words than a narrow pane can show";
    app.push_block(ScrollBlock::Thinking {
        location_key: "t".into(),
        text: format!("**{summary}**"),
        sealed: true,
        elapsed_ms: Some(2_100),
        mode: DisplayMode::Collapsed,
        manual: false,
    });
    paint(&mut app, 40);
    assert_eq!(app.tool_hit_rows, [] as [(usize, u16); 0]);
    app.toggle_tool_expand_at(0);
    assert_eq!(
        app.pane.blocks[0].display_mode(),
        Some(DisplayMode::Collapsed)
    );
    let body = app.transcript_body_area.unwrap();
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        body.y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.right() - 1,
        body.y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.right() - 1,
        body.y,
    ));
    paint(&mut app, 40);
    assert_eq!(
        app.take_transcript_selection_text(),
        Some(format!("◆ {summary}"))
    );
}

#[test]
fn cell_more_row_opens_only_the_cell_and_remains_open_on_a_second_click() {
    let mut app = transcript_fixture();
    let commands = (0..11).map(|i| format!("command-{i}")).collect::<Vec<_>>();
    let calls = commands
        .iter()
        .map(|cmd| ("exec_command", "done", cmd.as_str(), "preview"))
        .collect::<Vec<_>>();
    super::test_support::compose_cell(
        &mut app,
        "cell",
        "exec_js",
        "done",
        "11 tools",
        "const x = 1;",
        &calls,
    );
    paint(&mut app, 80);
    assert!(plain(&app).contains("… 1 more"));
    for _ in 0..2 {
        let hit = app
            .tool_hit_targets
            .iter()
            .position(|t| *t == RowTarget::CellMore(0))
            .unwrap();
        let (_, y) = app.tool_hit_rows[hit];
        let body = app.transcript_body_area.unwrap();
        app.handle_mouse(left_mouse(
            MouseEventKind::Down(MouseButton::Left),
            body.x + 8,
            y,
        ));
        app.handle_mouse(left_mouse(
            MouseEventKind::Up(MouseButton::Left),
            body.x + 8,
            y,
        ));
        paint(&mut app, 80);
        assert_eq!(
            app.pane.blocks[0].display_mode(),
            Some(DisplayMode::Expanded)
        );
        assert!(
            app.pane.blocks[1..]
                .iter()
                .all(|block| block.display_mode() == Some(DisplayMode::Collapsed))
        );
        assert!(plain(&app).contains("output ▸"));
    }
}

#[test]
fn live_cell_outline_repaints_do_not_replan_and_keep_narrow_item_geometry() {
    let mut app = transcript_fixture();
    super::test_support::compose_cell(
        &mut app,
        "cell",
        "exec_js",
        "running",
        "1 tools",
        "const x = 1;",
        &[(
            "exec_command",
            "running",
            "a long command with arguments and more arguments",
            "preview",
        )],
    );
    paint(&mut app, 80);
    crate::tui::activity::PLAN_CALLS.set(0);
    paint(&mut app, 80);
    paint(&mut app, 26);
    assert_eq!(crate::tui::activity::PLAN_CALLS.get(), 0);
    assert!(
        app.pane
            .transcript_cache
            .plain()
            .iter()
            .all(|row| unicode_width::UnicodeWidthStr::width(row.as_str()) <= 25)
    );
    let row = app
        .pane
        .transcript_cache
        .plain()
        .iter()
        .find(|row| row.contains("$ a long"))
        .unwrap();
    assert!(row.contains('…') && row.ends_with('▸'), "{row}");
}

#[test]
fn hidden_thoughts_are_not_keyboard_fold_targets() {
    let mut app = transcript_fixture();
    app.render_prefs.show_thinking = false;
    app.push_block(ScrollBlock::Thinking {
        location_key: "hidden".into(),
        text: "summary\nbody".into(),
        sealed: true,
        elapsed_ms: None,
        mode: DisplayMode::Collapsed,
        manual: false,
    });
    paint(&mut app, 80);
    assert_eq!(plain(&app), "");
    assert_eq!(app.focused_foldable_block(), None);
}

#[test]
fn review_thought_led_group_keeps_header_focus_for_click_enter_and_ctrl_o() {
    let mut app = transcript_fixture();
    app.push_block(ScrollBlock::Thinking {
        location_key: "t".into(),
        text: "summary\nbody".into(),
        sealed: true,
        elapsed_ms: None,
        mode: DisplayMode::Collapsed,
        manual: false,
    });
    for id in ["a", "b"] {
        app.apply_host_event(tool_line(
            id,
            None,
            None,
            ActivityKind::Tool,
            "read_file",
            "done",
            Some(id),
        ));
    }
    say(&mut app, "Done");
    for key in [
        super::test_support::key(KeyCode::Enter),
        super::test_support::key_ctrl(KeyCode::Char('o')),
    ] {
        paint(&mut app, 80);
        let hit = app
            .tool_hit_targets
            .iter()
            .position(|t| *t == RowTarget::Group)
            .unwrap();
        let (_, y) = app.tool_hit_rows[hit];
        let body = app.transcript_body_area.unwrap();
        app.handle_mouse(left_mouse(
            MouseEventKind::Down(MouseButton::Left),
            body.x + 4,
            y,
        ));
        app.handle_mouse(left_mouse(
            MouseEventKind::Up(MouseButton::Left),
            body.x + 4,
            y,
        ));
        app.handle_mouse(left_mouse(MouseEventKind::Moved, 0, 39));
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.pane.focused_block, Some(0));
        let header = app
            .tool_hit_rows
            .iter()
            .zip(&app.tool_hit_targets)
            .find(|(_, t)| **t == RowTarget::Group)
            .unwrap()
            .0
            .1;
        assert_eq!(
            terminal.backend().buffer()[(body.x, header)].bg,
            app.theme.row_focus.bg.unwrap()
        );
        app.handle_key(key);
        paint(&mut app, 80);
        assert!(app.pane.transcript_cache.row_plans()[0].group_folded);
    }
}

#[test]
fn review_running_failed_cell_keeps_spinner_and_output_copy_drops_rail() {
    let mut app = transcript_fixture();
    super::test_support::compose_cell(
        &mut app,
        "cell",
        "exec_js",
        "running",
        "1 tools · 1 failed",
        "const x = 1;",
        &[("exec_command", "failed:exit 2", "false", "preview")],
    );
    paint(&mut app, 80);
    assert!(!plain(&app).lines().next().unwrap().contains('✗'));
    app.toggle_tool_expand_at(0);
    paint(&mut app, 80);
    let hit = app
        .tool_hit_targets
        .iter()
        .position(|t| *t == RowTarget::CellOutput(0))
        .unwrap();
    let (_, y) = app.tool_hit_rows[hit];
    let body = app.transcript_body_area.unwrap();
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.right() - 1,
        y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.right() - 1,
        y,
    ));
    paint(&mut app, 80);
    assert_eq!(
        app.take_transcript_selection_text(),
        Some("output ▸".into())
    );
}

#[test]
fn review_nested_cells_keep_source_and_output_footer_owners() {
    let mut app = transcript_fixture();
    for (id, parent, kind, subject, detail) in [
        (
            "outer",
            None,
            ActivityKind::Compose,
            "1 tools",
            "const outer = 1;",
        ),
        (
            "inner",
            Some("outer"),
            ActivityKind::Compose,
            "1 tools",
            "const inner = 2;",
        ),
        (
            "grandchild",
            Some("inner"),
            ActivityKind::Process,
            "pwd",
            "preview",
        ),
    ] {
        app.apply_host_event(tool_line(
            id,
            parent,
            Some(id),
            kind,
            if kind == ActivityKind::Compose {
                "exec_js"
            } else {
                "exec_command"
            },
            "done",
            Some(subject),
        ));
        if let ScrollBlock::Tool { detail: stored, .. } = app.pane.blocks.last_mut().unwrap() {
            *stored = Some(detail.into());
        }
    }
    app.toggle_tool_expand_at(0);
    app.toggle_tool_expand_at(1);
    let _ = requests(&mut app);
    app.apply_host_event(canonical("outer", "{}", "OUTER OUTPUT"));
    app.apply_host_event(canonical("inner", "{}", "INNER OUTPUT"));
    paint(&mut app, 100);
    let rows = plain(&app);
    assert!(rows.find("$ pwd").unwrap() < rows.find("const inner").unwrap());
    assert!(rows.find("const inner").unwrap() < rows.find("const outer").unwrap());
    for (owner, output) in [(1, "INNER OUTPUT"), (0, "OUTER OUTPUT")] {
        let hit = app
            .tool_hit_targets
            .iter()
            .position(|t| *t == RowTarget::CellOutput(owner))
            .unwrap();
        let (block, y) = app.tool_hit_rows[hit];
        let body = app.transcript_body_area.unwrap();
        assert_eq!(block, 2);
        app.handle_mouse(left_mouse(
            MouseEventKind::Down(MouseButton::Left),
            body.x + 8,
            y,
        ));
        app.handle_mouse(left_mouse(
            MouseEventKind::Up(MouseButton::Left),
            body.x + 8,
            y,
        ));
        paint(&mut app, 100);
        assert!(plain(&app).contains(output));
    }
}
