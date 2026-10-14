//! Resume picker tests.

use super::terminal::dispatch_action;
use super::test_support::{
    FIXED_NOW, key, launch_basic, left_click, left_mouse, provider_rows, restore_snapshot,
    resume_fixture, serialize_frame, tool_item, transcript_scenario, usage_fixture,
};
use super::{App, DisplayMode};
use crate::tui::blocks::ScrollBlock;
use crate::tui::presentation::ActivityKind;
use crate::tui::{HostEvent, ResumeMenu, ResumeSessionEntry, SettingsSnapshot, UserAction};
use crate::tui::{ResumeRow as _, SurfaceObservation};
use crossterm::event::{KeyCode, MouseEventKind};
use plakous_host_protocol::views::transcript::{NoticeScope, ToolItem, TranscriptRow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[test]
fn opening_note_stays_before_later_turns_across_snapshots() {
    for initial in [Vec::new(), vec![TranscriptRow::assistant("old answer")]] {
        let mut launch = launch_basic();
        launch.opening_note = Some("opened here".into());
        let mut app = App::new(launch, true);
        app.apply_host_event(restore_snapshot(1, initial.clone()));
        let opening = app.pane.blocks.clone();
        let mut rows = initial;
        for revision in 2..=4 {
            let user = TranscriptRow::user(format!("turn {revision}"));
            let answer = TranscriptRow::assistant(format!("answer {revision}"));
            app.apply_host_event(HostEvent::Row(user.clone()));
            app.apply_host_event(HostEvent::Row(answer.clone()));
            rows.extend([user, answer]);
            app.apply_host_event(restore_snapshot(revision, rows.clone()));
            assert_eq!(&app.pane.blocks[..opening.len()], opening.as_slice());
            assert_eq!(app.pane.blocks.len(), rows.len() + 1);
            assert_eq!(
                app.pane
                    .blocks
                    .iter()
                    .filter(|block| matches!(block,
                ScrollBlock::System(text) if text == "opened here"))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn opening_note_tracks_front_evictions_in_a_full_snapshot_window() {
    use plakous_host_protocol::views::presentation::MAX_SNAPSHOT_ROWS;
    let mut launch = launch_basic();
    launch.opening_note = Some("opened here".into());
    let mut app = App::new(launch, true);
    let mut rows = (0..MAX_SNAPSHOT_ROWS - 2)
        .map(|index| TranscriptRow::assistant(format!("old row {index}")))
        .collect::<Vec<_>>();
    rows.extend([
        TranscriptRow::Tool(tool_item("last-pre-open-call", "read_file")),
        TranscriptRow::assistant("last old answer"),
    ]);
    app.apply_host_event(restore_snapshot(1, rows.clone()));
    for revision in 2..=5 {
        rows.drain(..2);
        rows.extend([
            TranscriptRow::user(format!("new turn {revision}")),
            TranscriptRow::assistant(format!("new answer {revision}")),
        ]);
        assert_eq!(rows.len(), MAX_SNAPSHOT_ROWS);
        app.apply_host_event(restore_snapshot(revision, rows.clone()));
        let note = app
            .pane
            .blocks
            .iter()
            .position(|block| matches!(block, ScrollBlock::System(text) if text == "opened here"))
            .expect("pre-open anchor is still present");
        assert!(
            matches!(&app.pane.blocks[note - 1], ScrollBlock::Assistant(text)
            if text == "last old answer")
        );
        assert!(matches!(
            &app.pane.blocks[note + 1],
            ScrollBlock::User { .. }
        ));
        assert!(
            !app.pane.blocks[..note]
                .iter()
                .any(|block| matches!(block, ScrollBlock::User { .. }))
        );
    }
    let later = rows
        .into_iter()
        .filter(|row| row.text().is_some_and(|text| text.starts_with("new")))
        .collect::<Vec<_>>();
    app.apply_host_event(restore_snapshot(6, later));
    assert!(
        app.pane
            .blocks
            .iter()
            .all(|block| !matches!(block, ScrollBlock::System(text) if text == "opened here"))
    );
}

#[test]
fn opening_note_is_dropped_when_anonymous_prefix_cannot_be_proved() {
    use plakous_host_protocol::views::presentation::MAX_SNAPSHOT_ROWS;
    for initial in [
        vec![TranscriptRow::assistant("old answer")],
        vec![TranscriptRow::assistant("repeated answer"); MAX_SNAPSHOT_ROWS],
    ] {
        let mut launch = launch_basic();
        launch.opening_note = Some("opened here".into());
        let mut app = App::new(launch, true);
        app.apply_host_event(restore_snapshot(1, initial.clone()));
        let replacement = if initial.len() == MAX_SNAPSHOT_ROWS {
            initial.clone()
        } else {
            vec![TranscriptRow::assistant("different prefix")]
        };
        app.apply_host_event(restore_snapshot(2, replacement));
        assert!(app.opening_note.is_none());
        app.apply_host_event(restore_snapshot(3, initial));
        assert!(
            app.pane
                .blocks
                .iter()
                .all(|block| !matches!(block, ScrollBlock::System(text) if text == "opened here"))
        );
    }
}

#[test]
fn projected_tool_rows_replace_previews_and_restored_rows_in_place() {
    let mut tool = tool_item("call", "exec_command");
    tool.state = "done".into();
    tool.started_at_ms = Some(1_000);
    tool.finished_at_ms = Some(1_018);
    let before = vec![
        TranscriptRow::Tool(tool.clone()),
        TranscriptRow::assistant("answer"),
    ];
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(1, before));
    tool.finished_at_ms = Some(1_037);
    let row = TranscriptRow::Tool(tool);
    app.apply_host_event(HostEvent::Row(row.clone()));
    app.apply_host_event(HostEvent::Row(row.clone()));
    assert_eq!(app.pane.blocks.len(), 2);
    assert_eq!(app.pane.blocks[0], ScrollBlock::from_row(row).unwrap());
    assert_eq!(app.pane.blocks[1], ScrollBlock::Assistant("answer".into()));
}

#[test]
fn projected_settlements_keep_the_operators_tool_fold_choices() {
    let mut tool = tool_item("call", "exec_command");
    tool.state = "done".into();
    let rows = vec![
        TranscriptRow::Tool(tool.clone()),
        TranscriptRow::assistant("answer"),
    ];
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(restore_snapshot(1, rows.clone()));
    app.toggle_all_blocks();
    let opened = app.pane.blocks[0].clone();
    app.apply_host_event(HostEvent::Row(TranscriptRow::Tool(tool)));
    assert_eq!(app.pane.blocks[0], opened);
    app.apply_host_event(restore_snapshot(2, rows));
    assert_eq!(app.pane.blocks[0], opened);
}

#[test]
fn a_late_resume_reply_never_reopens_a_dismissed_or_shadowed_picker() {
    fn entries() -> Vec<ResumeSessionEntry> {
        vec![ResumeSessionEntry {
            id: "session-1".into(),
            account: Some("acct".into()),
            model: Some("model-x".into()),
            effort: None,
            extra: None,
            first_prompt: Some("first session".into()),
            in_place: true,
            ..ResumeSessionEntry::default()
        }]
    }

    // An unrequested snapshot is dropped outright.
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::ResumeSessions(entries()));
    assert!(
        app.resume_menu.is_none(),
        "no /resume was outstanding, so nothing opens"
    );

    // The requested reply opens the picker.
    let mut app = App::new(launch_basic(), true);
    dispatch_action(&mut app, &UserAction::ResumeSession { id: None }, None);
    assert!(app.pending_open_resume, "the request is outstanding");
    app.apply_host_event(HostEvent::ResumeSessions(entries()));
    assert!(app.resume_menu.is_some(), "the requested picker opens");
    assert!(!app.pending_open_resume, "and the request is retired");

    // Esc before the reply lands: the late snapshot must not reopen it.
    let mut app = App::new(launch_basic(), true);
    dispatch_action(&mut app, &UserAction::ResumeSession { id: None }, None);
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.pending_open_resume, "Esc retires the request");
    app.apply_host_event(HostEvent::ResumeSessions(entries()));
    assert!(
        app.resume_menu.is_none(),
        "a late reply after Esc must not reopen the picker"
    );

    // A newer modal owns input: the reply must not stack over it.
    let mut app = App::new(launch_basic(), true);
    dispatch_action(&mut app, &UserAction::ResumeSession { id: None }, None);
    app.info_overlay = Some(("help".into(), "body".into()));
    app.apply_host_event(HostEvent::ResumeSessions(entries()));
    assert!(
        app.resume_menu.is_none(),
        "a picker must not stack over the modal that owns the key chain"
    );
}

#[test]
fn resume_mouse_selects_activates_scrolls_and_does_not_leak() {
    let mut app = App::new(launch_basic(), true);
    let entries = vec![
        ResumeSessionEntry {
            id: "first".into(),
            account: Some("acct".into()),
            model: Some("model-x".into()),
            effort: Some("high".into()),
            extra: None,
            first_prompt: Some("first session".into()),
            in_place: false,
            ..ResumeSessionEntry::default()
        },
        ResumeSessionEntry {
            id: "second".into(),
            account: Some("other".into()),
            model: Some("model-y".into()),
            effort: Some("low".into()),
            extra: None,
            first_prompt: Some("second session".into()),
            in_place: false,
            ..ResumeSessionEntry::default()
        },
    ];
    app.resume_menu = Some(ResumeMenu::open(
        entries.clone(),
        FIXED_NOW,
        jiff::tz::TimeZone::UTC,
    ));
    let backend = TestBackend::new(90, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let second = app.resume_hits[1].rect;
    app.handle_mouse(left_mouse(MouseEventKind::ScrollDown, second.x, second.y));
    assert_eq!(app.resume_menu.as_ref().map(|menu| menu.selected), Some(1));
    assert_eq!(
        app.handle_mouse(left_click(second.x, second.y)),
        None,
        "a click on the focused row only keeps the focus"
    );
    assert_eq!(
        app.handle_mouse(left_click(second.x, second.y)),
        Some(UserAction::ResumeSessionHandoff {
            id: "second".into()
        })
    );

    app.resume_menu = Some(ResumeMenu::open(
        entries,
        FIXED_NOW,
        jiff::tz::TimeZone::UTC,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let composer = app.composer_inner.expect("composer");
    let cursor = app.input_cursor;
    assert!(
        app.handle_mouse(left_click(composer.x, composer.y))
            .is_none()
    );
    assert!(app.resume_menu.is_none(), "outside click dismisses modal");
    assert_eq!(
        app.input_cursor, cursor,
        "dismissal must not reach composer"
    );
}

#[test]
fn a_compose_cell_keeps_its_outline_live_and_after_resume() {
    let mut live = App::new(launch_basic(), true);
    super::test_support::compose_cell(
        &mut live,
        "cell",
        "exec_js",
        "done",
        "1 tools",
        "const result = 1;",
        &[("exec_command", "done", "pwd", "RAW OUTPUT")],
    );
    let item = |id: &str, parent: Option<&str>, name: &str, subject: &str, detail: &str| {
        TranscriptRow::Tool(ToolItem {
            parent_call_id: parent.map(str::to_owned),
            cell_id: Some("cell".into()),
            subject: Some(subject.into()),
            detail: Some(detail.into()),
            ..tool_item(id, name)
        })
    };
    let mut restored = App::new(launch_basic(), true);
    restored.apply_host_event(restore_snapshot(
        1,
        vec![
            item("cell", None, "exec_js", "1 tools", "const result = 1;"),
            item(
                "cell-child-0",
                Some("cell"),
                "exec_command",
                "pwd",
                "RAW OUTPUT",
            ),
        ],
    ));
    for app in [&mut live, &mut restored] {
        let plans = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs);
        assert!(!plans[1].hidden);
        app.toggle_tool_expand_at(0);
        let rows =
            crate::tui::blocks::layout_blocks_with(&app.pane.blocks, 80, app.render_prefs).plain;
        assert!(rows.iter().any(|row| row.contains("const result")));
        assert!(rows.iter().any(|row| row.contains("$ pwd")));
        assert!(!rows.iter().any(|row| row.contains("RAW OUTPUT")));
        app.toggle_tool_expand_at(0);
        assert!(!crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[1].hidden);
    }
}

#[test]
fn a_resumed_snapshot_restores_sealed_thinking_between_user_and_tools() {
    let mut resumed = App::new(launch_basic(), true);
    resumed.apply_host_event(restore_snapshot(
        1,
        vec![
            TranscriptRow::user("survey the repo"),
            TranscriptRow::Thinking(plakous_host_protocol::views::transcript::ThinkingRow {
                location_key: "0:0".into(),
                text: "inventory the repo".into(),
                elapsed_ms: Some(4_100),
            }),
            TranscriptRow::Tool(ToolItem {
                cell_id: Some("cell-1".into()),
                subject: Some("1 tool".into()),
                started_at_ms: Some(1_000),
                finished_at_ms: Some(1_088),
                ..tool_item("cell-1", "exec")
            }),
        ],
    ));
    assert!(matches!(
        &resumed.pane.blocks[1],
        ScrollBlock::Thinking {
            text,
            sealed: true,
            elapsed_ms: Some(4_100),
            mode: DisplayMode::Collapsed,
            ..
        } if text == "inventory the repo"
    ));
    assert!(matches!(
        &resumed.pane.blocks[2],
        ScrollBlock::Tool {
            kind: ActivityKind::Compose,
            ..
        }
    ));
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| resumed.draw(frame)).unwrap();
    let frame = serialize_frame(terminal.backend().buffer());
    assert!(frame.contains("survey·the·repo"));
    assert!(frame.contains("◆·inventory·the·repo"));
    assert!(frame.contains("CodeMode···1·call"));
}

#[test]
fn resume_snapshot_selects_full_host_id() {
    let mut app = App::new(launch_basic(), true);
    let first = "018f2537-5ed2-7b11-a4b1-f009be85f101";
    let second = "018f2537-5ed2-7b11-a4b1-f009be85f202";
    // The picker only opens for an outstanding request.
    app.pending_open_resume = true;
    app.apply_host_event(HostEvent::ResumeSessions(vec![
        ResumeSessionEntry {
            id: first.into(),
            account: Some("codex".into()),
            model: Some("gpt-5".into()),
            effort: Some("high".into()),
            extra: Some("Fast".into()),
            first_prompt: Some("first".into()),
            in_place: false,
            ..ResumeSessionEntry::default()
        },
        ResumeSessionEntry {
            id: second.into(),
            account: Some("grok".into()),
            model: Some("grok-code".into()),
            effort: Some("high".into()),
            extra: None,
            first_prompt: Some("second".into()),
            in_place: false,
            ..ResumeSessionEntry::default()
        },
    ]));
    assert!(app.resume_menu.is_some());
    assert!(app.handle_key(key(KeyCode::Down)).is_none());
    // The picker's `Enter` resumes the moment it is pressed.
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::ResumeSessionHandoff { id: second.into() })
    );
    assert!(app.resume_menu.is_none());
    assert_eq!(app.session_id.as_deref(), Some("s1"));
}

#[test]
fn picker_resume_stays_in_place_when_current_host_owns_exact_binding() {
    let mut app = App::new(launch_basic(), true);
    let exact = "018f2537-5ed2-7b11-a4b1-f009be85f303";
    // The picker only opens for an outstanding request.
    app.pending_open_resume = true;
    app.apply_host_event(HostEvent::ResumeSessions(vec![ResumeSessionEntry {
        id: exact.into(),
        account: Some("acct".into()),
        model: Some("model-x".into()),
        effort: Some("high".into()),
        extra: None,
        first_prompt: Some("same".into()),
        in_place: true,
        ..ResumeSessionEntry::default()
    }]));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::ResumeSession {
            id: Some(exact.into())
        })
    );
}

#[test]
fn resume_snapshot_identity_remains_opaque_until_product_handoff() {
    let mut app = App::new(launch_basic(), true);
    // The picker only opens for an outstanding request.
    app.pending_open_resume = true;
    app.apply_host_event(HostEvent::ResumeSessions(vec![ResumeSessionEntry {
        id: "../not-a-session".into(),
        account: None,
        model: None,
        effort: None,
        extra: None,
        in_place: false,
        ..ResumeSessionEntry::default()
    }]));
    assert!(app.resume_menu.is_some());
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::ResumeSessionHandoff {
            id: "../not-a-session".into()
        })
    );
}

#[test]
fn empty_resume_snapshot_closes_without_action() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::ResumeSessions(Vec::new()));
    assert!(app.handle_key(key(KeyCode::Enter)).is_none());
    assert!(app.resume_menu.is_none());
}

/// One row per session — short id, relative age,
/// the opening prompt, and the binding as a dim right-aligned value —
/// newest first, in the shared modal chrome with an `Enter resume · Esc
/// close` footer.
#[test]
fn the_resume_picker_matches_reviewable_goldens_at_both_sizes() {
    for (width, height) in [(80u16, 24u16), (120, 40)] {
        let mut app = resume_fixture();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let actual = serialize_frame(terminal.backend().buffer());
        let path = format!("tests/goldens/resume-{width}x{height}.frame");
        if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
            std::fs::write(&path, &actual).expect("write reviewed frame golden");
        } else {
            let expected = std::fs::read_to_string(&path).expect("golden frame");
            assert_eq!(actual, expected, "{path}");
        }
    }
}

/// The host sorts; the picker preserves. Row order, selection and the ages
/// each row shows all follow the snapshot the host sent.
#[test]
fn the_picker_preserves_the_hosts_newest_first_order() {
    let app = resume_fixture();
    let menu = app.resume_menu.as_ref().expect("picker");
    let ages: Vec<String> = (0..menu.entries.len())
        .map(|index| menu.entries[index].display_age(menu.now_unix, &menu.zone))
        .collect();
    assert_eq!(
        ages,
        ["2m ago", "3h ago", "yesterday", "4d ago", "2026-08-04"]
    );
    let ids: Vec<String> = (0..menu.entries.len())
        .map(|index| menu.entries[index].short_id())
        .collect();
    assert_eq!(
        ids,
        ["018f2537", "7c1d0e94", "b2a4f6c1", "4d5e6f70", "9a8b7c6d"]
    );
    assert_eq!(menu.selected, 0, "the newest session is preselected");
    // Every row is one line: the label carries the identity, and only the
    // summary is allowed to be long.
    for (index, id) in ids.iter().enumerate() {
        let label = menu.row_label(index).expect("row label");
        assert!(!label.contains('\n'), "row {index} is one line");
        assert!(label.starts_with(id), "row {index} leads with id");
    }
    assert_eq!(
        menu.row_label(3).as_deref(),
        Some("4d5e6f70  4d ago      (no prompt yet)"),
        "a session nobody prompted says so rather than showing an empty column"
    );
    assert!(
        menu.row_label(4)
            .is_some_and(|row| row.ends_with("Nightly triage")),
        "a titled session with no prompt falls back to its title"
    );
}

/// Spec 02 §5.5: an unfocused row focuses on the first click and activates on
/// the second; the value column activates in one.
#[test]
fn resume_rows_activate_on_double_click_and_the_binding_column_is_one_click() {
    let mut app = resume_fixture();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let third = app.resume_hits[2];
    assert_eq!(
        app.handle_mouse(left_click(third.rect.x + 4, third.rect.y)),
        None,
        "a click only focuses the row"
    );
    assert_eq!(app.resume_menu.as_ref().map(|menu| menu.selected), Some(2));
    assert_eq!(
        app.handle_mouse(left_click(third.rect.x + 4, third.rect.y)),
        Some(UserAction::ResumeSessionHandoff {
            id: "b2a4f6c1-77de-4a90-8c31-1e2d3f405162".into()
        }),
        "a double-click activates it"
    );

    // The value column focuses and activates in one click.
    let mut app = resume_fixture();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let last = app.resume_hits[4];
    assert_eq!(
        app.handle_mouse(left_click(last.value.x, last.rect.y)),
        Some(UserAction::ResumeSessionHandoff {
            id: "9a8b7c6d-5e4f-4302-9182-736455647382".into()
        })
    );

    // Hover highlights and nothing else, so a click still lands on an
    // unfocused row under the pointer.
    let mut app = resume_fixture();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let second = app.resume_hits[1].rect;
    app.handle_mouse(left_mouse(MouseEventKind::Moved, second.x + 4, second.y));
    assert_eq!(app.resume_hover, Some(1));
    assert_eq!(
        app.resume_menu.as_ref().map(|menu| menu.selected),
        Some(0),
        "hover never moves focus"
    );

    // The close glyph on the border dismisses without resuming anything.
    let close = app.resume_close_hit.expect("close glyph");
    assert_eq!(app.handle_mouse(left_click(close.x, close.y)), None);
    assert!(app.resume_menu.is_none());
}

/// Keyboard and mouse resolve the same exact identity, and `Enter` on an
/// in-place binding resumes without a handoff.
#[test]
fn resume_keyboard_selects_the_same_rows_the_mouse_does() {
    let mut app = resume_fixture();
    assert!(app.handle_key(key(KeyCode::Down)).is_none());
    assert!(app.handle_key(key(KeyCode::Down)).is_none());
    assert_eq!(app.resume_menu.as_ref().map(|menu| menu.selected), Some(2));
    assert!(app.handle_key(key(KeyCode::Up)).is_none());
    assert!(app.handle_key(key(KeyCode::Up)).is_none());
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::ResumeSession {
            id: Some("018f2537-5ed2-7b11-a4b1-f009be85f101".into())
        }),
        "the in-place row resumes without a terminal handoff"
    );
}

/// Spec 02 §5.4 goldens at both review sizes: the Compose row in every form, one row per §5.2 header format, the thought block,
/// and the spacing rule (a blank row between text and the tool row beneath
/// it, and between consecutive Compose cells).
#[test]
fn the_transcript_matches_reviewable_goldens_at_both_sizes() {
    for scenario in [
        "compose-collapsed",
        "compose-expanded-javascript",
        "compose-expanded-python",
        "compose-inflight",
        "compose-cancelled",
        "tool-rows",
        "thought",
        "spacing",
    ] {
        for (width, height) in [(80u16, 24u16), (120, 40)] {
            let mut app = transcript_scenario(scenario);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let actual = serialize_frame(terminal.backend().buffer());
            let path = format!("tests/goldens/transcript-{scenario}-{width}x{height}.frame");
            if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
                std::fs::write(&path, &actual).expect("write reviewed frame golden");
            } else {
                let expected = std::fs::read_to_string(&path).expect("golden frame");
                assert_eq!(actual, expected, "{path}");
            }
        }
    }
}

/// The live head and the row a `/resume` rebuilds report the same
/// duration for the same yield-and-wait cell, because both read the host's
/// `started_at_ms`/`finished_at_ms` pair instead of a renderer clock.
#[test]
fn a_yield_and_wait_cell_reports_one_duration_live_and_resumed() {
    const STARTED_AT_MS: u64 = 1_788_000_000_000;
    const FINISHED_AT_MS: u64 = STARTED_AT_MS + 45_300;

    let mut live = App::new(launch_basic(), true);
    let compose = |state: &str, duration_ms: Option<u64>| HostEvent::ToolLine {
        call_id: Some("exec-1".into()),
        parent_call_id: None,
        cell_id: Some("cell-1".into()),
        kind: ActivityKind::Compose,
        name: "exec".into(),
        state: state.into(),
        subject: Some("5 tools".to_owned()),
        detail: None,
        duration_ms,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    };
    live.apply_host_event(compose("running", None));
    // The yield, then the dispatch that reopens the cell, then its end.
    live.apply_host_event(compose("done", Some(16_200)));
    live.apply_host_event(compose("running", None));
    live.apply_host_event(compose("done", Some(FINISHED_AT_MS - STARTED_AT_MS)));
    let live_state = match live.pane.blocks.last() {
        Some(ScrollBlock::Tool { state, .. }) => state.clone(),
        other => panic!("expected a Compose row, got {other:?}"),
    };

    // The same cell as the transcript projection rebuilds it on reopen.
    let mut resumed = App::new(launch_basic(), true);
    resumed.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::Tool(ToolItem {
            cell_id: Some("cell-1".to_owned()),
            subject: Some("5 tools".to_owned()),
            started_at_ms: Some(STARTED_AT_MS),
            finished_at_ms: Some(FINISHED_AT_MS),
            ..tool_item("exec-1", "exec")
        })],
    ));
    let resumed_state = match resumed.pane.blocks.last() {
        Some(ScrollBlock::Tool { state, .. }) => state.clone(),
        other => panic!("expected a Compose row, got {other:?}"),
    };

    assert_eq!(live_state, "done:45.3s");
    assert_eq!(
        live_state, resumed_state,
        "live and resumed must report one cell duration"
    );
}

/// A sealed group's meta is its elapsed span, first call's start to last
/// call's finish on the host's stamps (spec 02 §5.4), not the sum of its
/// calls: two 2s commands 9s apart read `11.0s`, not `4.0s`, live and
/// restored. A live row the host sent no stamps for keeps the sum.
#[test]
fn a_sealed_group_meta_is_its_elapsed_span() {
    let command = |id: &str, subject: &str, started: Option<u64>| ToolItem {
        subject: Some(subject.into()),
        started_at_ms: started,
        finished_at_ms: started.map(|started| started + 2_000),
        ..tool_item(id, "exec_command")
    };
    let tool_line = |item: &ToolItem, state: &str| HostEvent::ToolLine {
        call_id: Some(item.call_id.clone()),
        parent_call_id: None,
        cell_id: None,
        kind: item.kind,
        name: item.name.clone(),
        state: state.into(),
        subject: item.subject.clone(),
        detail: None,
        duration_ms: (state == "done").then_some(2_000),
        started_at_ms: item.started_at_ms,
        finished_at_ms: (state == "done").then_some(item.finished_at_ms).flatten(),
        wait: None,
    };
    let summary = |app: &mut App| {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let frame = serialize_frame(terminal.backend().buffer());
        frame
            .lines()
            .find(|line| line.contains("Ran·2·commands"))
            .unwrap_or_else(|| panic!("{frame}"))
            .to_owned()
    };
    for (starts, elapsed) in [
        ((Some(1_000), Some(10_000)), "11.0s"),
        ((None, None), "4.0s"),
    ] {
        let calls = [command("a", "ls", starts.0), command("b", "pwd", starts.1)];
        let mut live = App::new(launch_basic(), true);
        live.apply_host_event(HostEvent::UserMessage {
            text: "Inspect.".into(),
            sent_at_ms: None,
        });
        for call in &calls {
            live.apply_host_event(tool_line(call, "running"));
            live.apply_host_event(tool_line(call, "done"));
        }
        live.apply_host_event(HostEvent::AssistantDelta("Done.".into()));
        live.apply_host_event(HostEvent::AssistantDone);
        let mut rows = vec![TranscriptRow::user("Inspect.")];
        rows.extend(calls.iter().cloned().map(TranscriptRow::Tool));
        rows.push(TranscriptRow::assistant("Done."));
        let mut restored = App::new(launch_basic(), true);
        restored.apply_host_event(restore_snapshot(1, rows));
        let live_summary = summary(&mut live);
        assert!(live_summary.contains(elapsed), "{elapsed}: {live_summary}");
        if starts.0.is_some() {
            assert_eq!(summary(&mut restored), live_summary);
        }
    }
}

/// Spec 02 §5.5: every one of the seven tabs has a reviewable golden at both
/// review sizes. The Providers golden carries all four verticals in mixed
/// states.
#[test]
fn every_control_center_tab_matches_reviewable_goldens_at_both_sizes() {
    for (tab, slug) in [
        (super::ControlTab::Settings, "settings"),
        (super::ControlTab::Models, "models"),
        (super::ControlTab::Providers, "providers"),
        (super::ControlTab::Usage, "usage"),
        (super::ControlTab::Context, "context"),
        (super::ControlTab::Session, "session"),
        (super::ControlTab::Keybinds, "keybinds"),
    ] {
        let mut sizes = vec![(80u16, 24u16), (120, 40)];
        // Wide terminals keep the full-page frame and center a 120-column body.
        if matches!(slug, "settings" | "keybinds") {
            sizes.push((200, 50));
        }
        for (width, height) in sizes {
            let mut app = usage_fixture();
            app.pending_open_settings = true;
            app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
                SettingsSnapshot::default(),
            )));
            app.providers = provider_rows();
            app.activate_control_tab(tab);
            if tab == super::ControlTab::Models {
                app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(
                    super::test_support::builtin_profiles_view(),
                )));
            }
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let actual = serialize_frame(terminal.backend().buffer());
            let path = format!("tests/goldens/tab-{slug}-{width}x{height}.frame");
            if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
                std::fs::write(&path, &actual).expect("write reviewed frame golden");
            } else {
                let expected = std::fs::read_to_string(&path).expect("golden exists");
                assert_eq!(actual, expected, "{path}");
            }
        }
    }
}

/// An abandoned open that publishes no notice for its command still shows
/// exactly one recovery row, on the TUI and the JSONL face, and never stores it.
#[test]
fn abandoned_open_recovery_row_reaches_tui_and_jsonl_once() {
    use crate::observation_parity_tests::{Fixture, Script};
    use plakous_host_protocol::envelope::JsonlCorrelation;
    use plakous_host_protocol::jsonl::project_host_event;
    use plakous_hosted::runtime::spawn_host_worker;
    use plakous_hosted::sessions::QueueItem;

    for with_submission in [false, true] {
        let fixture = Fixture::new(Script::Effect);
        let mut config = fixture.host_config();
        let store = config.store.clone();
        let id = config.session.id.clone();
        let (actions, events, worker) = spawn_host_worker(config).unwrap();
        actions
            .send(UserAction::Prompt("seed terminal".into()))
            .unwrap();
        let mut started = false;
        loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap();
            if matches!(event, HostEvent::PromptStarted(_)) {
                started = true;
            }
            if started && matches!(event, HostEvent::Status { ref phase, .. } if phase == "idle") {
                break;
            }
        }
        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();
        config = fixture.host_config();
        config.session = store.load(&id).unwrap();
        let source = config.session.plakous_locator.unwrap();
        let database =
            crate::home::paths::PlakPaths::explicit(config.product_home.clone()).runtime_database();
        let executor = tokio::runtime::Runtime::new().unwrap();
        executor
            .block_on(plakous_offline_crash_harness::seed_fenced_code_cell(
                &database,
                source,
                "face-fenced-root",
                b"fenced face prompt",
                false,
            ))
            .unwrap();
        if with_submission {
            let item = QueueItem::new("fenced face prompt");
            config.session.pending_queue.push(item.clone());
            let submission = plakous_hosted::sessions::projector::Submission {
                command_id: "face-fenced-root".into(),
                kind: plakous_hosted::sessions::projector::SubmissionKind::RootStart,
                queue_item_id: item.id.clone(),
                queue_item_revision: item.revision,
                target_turn_id: None,
                canonical_command: item.text.clone(),
                submitted_at_ms: item.submitted_at_ms,
            };
            store
                .save_submission(&mut config.session, &submission)
                .unwrap();
        }
        store.save(&mut config.session).unwrap();
        let (actions, events, worker) = spawn_host_worker(config).unwrap();
        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();
        let mut app = App::new(launch_basic(), true);
        let mut jsonl_rows = 0;
        let mut recovered = 0;
        for event in events.try_iter() {
            if matches!(&event, HostEvent::Recovered { .. }) {
                recovered += 1;
            }
            if matches!(&event, HostEvent::Row(row) if row.text().is_some_and(
                |text| text.contains("recovered an interrupted turn")
            )) {
                let line = project_host_event(&event, JsonlCorrelation::default()).unwrap();
                assert!(line.contains("recovered an interrupted turn"));
                jsonl_rows += 1;
            }
            app.apply_host_event(event);
        }
        assert_eq!(recovered, 1);
        assert_eq!(jsonl_rows, 1);
        assert_eq!(app.pane.blocks.iter().filter(|block| matches!(
            block, ScrollBlock::System(text) if text.contains("recovered an interrupted turn")
        )).count(), 1, "the snapshot must neither erase nor duplicate the recovery row");
        let record = store.load(&id).unwrap();
        assert_eq!(
            record
                .transcript
                .iter()
                .filter(|row| row
                    .text()
                    .is_some_and(|text| text.contains("recovered an interrupted turn")))
                .count(),
            0,
            "the recovery-required terminal derived its row; abandoning it derives none (with submission: {with_submission})"
        );
    }
}

/// A reopen says what it recovered before it publishes the transcript
/// snapshot, and the snapshot replaces the transcript. The note the host
/// published during the open must survive that replacement; a note from
/// before the open (or an unrelated live turn) is not carried.
#[test]
fn notes_published_during_an_open_survive_the_presentation_snapshot() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Row(TranscriptRow::info(
        NoticeScope::Host,
        "stale note from before the open",
    )));
    app.apply_host_event(HostEvent::SessionOpening);
    app.apply_host_event(HostEvent::Row(TranscriptRow::info(
        NoticeScope::Host,
        "plak: recovered an interrupted turn; nothing from it was kept",
    )));
    app.apply_host_event(HostEvent::Row(TranscriptRow::error(
        NoticeScope::Host,
        "restored turn requires recovery",
    )));
    app.apply_host_event(HostEvent::SessionOpened {
        session_open_ms: Some(1),
    });
    app.apply_host_event(restore_snapshot(
        1,
        vec![TranscriptRow::user("the interrupted prompt")],
    ));
    let texts: Vec<String> = app
        .pane
        .blocks
        .iter()
        .map(|block| match block {
            ScrollBlock::User { text, .. }
            | ScrollBlock::System(text)
            | ScrollBlock::Error(text) => text.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "the interrupted prompt".to_owned(),
            "plak: recovered an interrupted turn; nothing from it was kept".to_owned(),
            "restored turn requires recovery".to_owned(),
        ],
        "the open's notes follow the restored transcript, once; older notes are not carried"
    );
    // A later snapshot with no open in between carries nothing.
    app.apply_host_event(HostEvent::Row(TranscriptRow::info(
        NoticeScope::Host,
        "a live note",
    )));
    app.apply_host_event(restore_snapshot(2, Vec::new()));
    assert!(app.pane.blocks.is_empty(), "{:?}", app.pane.blocks);
}

#[test]
fn a_refused_restore_reports_its_bound_without_erasing_the_visible_transcript() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Row(TranscriptRow::info(
        NoticeScope::Host,
        "retained transcript",
    )));
    app.apply_host_event(restore_snapshot(
        1,
        (0..=crate::tui::presentation::MAX_SNAPSHOT_ROWS)
            .map(|_| TranscriptRow::info(NoticeScope::Turn, "notice"))
            .collect(),
    ));
    assert!(
        matches!(&app.pane.blocks[0], ScrollBlock::System(text) if text == "retained transcript")
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let frame = serialize_frame(terminal.backend().buffer());
    assert!(frame.contains("retained·transcript"));
    assert!(frame.contains("invalid·presentation·snapshot·refused:"));
    assert!(frame.contains("ItemLimit"));
}

/// Every transcript row type paints the same live and after a restore, in
/// both themes at the owner's two review widths. Live, the host emits each row
/// the way it does in a session (prompt, streamed prose and reasoning, tool
/// lines with their wait facts, `Row` for the rest); restored, the same rows
/// arrive as one presentation snapshot.
#[test]
fn every_row_type_paints_the_same_live_and_restored() {
    use plakous_host_protocol::views::transcript::{
        AgentMilestone, AutoContinue, CarriedFrom, MilestoneEvent, ProcessNotice, ProcessOutcome,
        RetryItem, ThinkingRow, WaitFacts,
    };
    let header = TranscriptRow::SessionHeader {
        session_id: "0a1b2c3d-0000".into(),
        carried_from: Some(CarriedFrom {
            session_id: "4e5f6a7b-0000".into(),
            state_key: "r22.e20.p29.t4".into(),
        }),
    };
    let wait = WaitFacts {
        timeout_ms: Some(3_600_000),
        woke_by: Some("c39 finished".into()),
        waited_ms: Some(720_000),
        settled_agent: Some("c39".into()),
    };
    let waited = ToolItem {
        subject: Some("c39".into()),
        started_at_ms: Some(1_000),
        finished_at_ms: Some(721_000),
        wait: Some(wait.clone()),
        ..tool_item("wait-1", "wait_agent")
    };
    let shell = ToolItem {
        subject: Some("cargo test".into()),
        started_at_ms: Some(2_000),
        finished_at_ms: Some(3_200),
        ..tool_item("shell-1", "exec_command")
    };
    let milestone = |event, text: Option<&str>| AgentMilestone {
        agent_id: "c39".into(),
        event,
        template: Some("worker".into()),
        text: text.map(str::to_owned),
        duration_ms: Some(840_000),
        candidate: Some("cand-c39-a1".into()),
        files: Some(16),
        paths: vec!["src/lib.rs".into()],
    };
    let rows_after_header = vec![
        TranscriptRow::user("Inspect."),
        TranscriptRow::User {
            text: "Inspect again.".into(),
            sent_at_ms: Some(1_735_828_320_000),
        },
        TranscriptRow::Thinking(ThinkingRow {
            location_key: "0:0".into(),
            text: "plan the wait".into(),
            elapsed_ms: Some(0),
        }),
        TranscriptRow::Tool(shell.clone()),
        TranscriptRow::Process(ProcessNotice {
            job_id: "12".into(),
            outcome: ProcessOutcome::Exited { code: Some(0) },
            duration_ms: Some(62_000),
            subject: None,
        }),
        TranscriptRow::Process(ProcessNotice {
            job_id: "p261".into(),
            outcome: ProcessOutcome::Exited { code: Some(0) },
            duration_ms: Some(1_300_000),
            subject: Some("just check".into()),
        }),
        TranscriptRow::Retry(RetryItem {
            attempt: 2,
            max_attempts: 4,
            cause: "429 rate limited".into(),
        }),
        TranscriptRow::Tool(waited.clone()),
        TranscriptRow::steer("also check docs"),
        TranscriptRow::Steer {
            text: "and the user guide".into(),
            sent_at_ms: Some(1_735_828_380_000),
        },
        TranscriptRow::Milestone(milestone(MilestoneEvent::Spawned, Some("brief\nmore"))),
        TranscriptRow::Milestone(milestone(MilestoneEvent::Retasked, Some("again"))),
        TranscriptRow::Milestone(milestone(MilestoneEvent::Finished, None)),
        TranscriptRow::Milestone(milestone(MilestoneEvent::Failed, Some("boom"))),
        TranscriptRow::Milestone(milestone(MilestoneEvent::NeedsRecovery, None)),
        TranscriptRow::Milestone(milestone(MilestoneEvent::Asks, Some("which branch?"))),
        TranscriptRow::Milestone(milestone(MilestoneEvent::Adopted, None)),
        TranscriptRow::Milestone(milestone(MilestoneEvent::AdoptConflict, None)),
        TranscriptRow::AutoContinue(AutoContinue {
            agents_finished: 2,
            processes_exited: 1,
            agents: vec!["c38".into(), "c39".into()],
        }),
        TranscriptRow::info(NoticeScope::Tree, "tree note"),
        TranscriptRow::warning(NoticeScope::Settings, "settings warning"),
        TranscriptRow::error(NoticeScope::Turn, "turn failed"),
        TranscriptRow::info(NoticeScope::Diagnostic, "session log only"),
        TranscriptRow::assistant("Finished."),
        TranscriptRow::Interrupted,
    ];
    let tool_lines = |item: &ToolItem| {
        let line =
            |state: &str, duration_ms: Option<u64>, wait: Option<WaitFacts>| HostEvent::ToolLine {
                call_id: Some(item.call_id.clone()),
                parent_call_id: None,
                cell_id: None,
                kind: item.kind,
                name: item.name.clone(),
                state: state.into(),
                subject: item.subject.clone(),
                detail: item.detail.clone(),
                duration_ms,
                started_at_ms: item.started_at_ms,
                finished_at_ms: (state != "running")
                    .then_some(item.finished_at_ms)
                    .flatten(),
                wait,
            };
        let started = item.wait.as_ref().map(|wait| WaitFacts {
            timeout_ms: wait.timeout_ms,
            ..WaitFacts::default()
        });
        [
            line("running", None, started),
            line("done", item.duration_ms(), item.wait.clone()),
        ]
    };
    for theme in [crate::tui::Theme::dark(), crate::tui::Theme::light()] {
        for width in [120, 40] {
            let mut live = App::new(launch_basic(), true);
            live.theme = theme.clone();
            live.apply_host_event(restore_snapshot(1, vec![header.clone()]));
            for row in &rows_after_header {
                match row {
                    TranscriptRow::User { text, sent_at_ms } => {
                        live.apply_host_event(HostEvent::UserMessage {
                            text: text.clone(),
                            sent_at_ms: *sent_at_ms,
                        });
                    }
                    TranscriptRow::Assistant { text } => {
                        live.apply_host_event(HostEvent::AssistantDelta(text.clone()));
                        live.apply_host_event(HostEvent::AssistantDone);
                    }
                    TranscriptRow::Thinking(thinking) => {
                        live.apply_host_event(HostEvent::ReasoningDelta {
                            location_key: thinking.location_key.clone(),
                            text: thinking.text.clone(),
                        });
                        live.apply_host_event(HostEvent::ReasoningSealed {
                            location_key: thinking.location_key.clone(),
                        });
                    }
                    TranscriptRow::Tool(item) => {
                        for line in tool_lines(item) {
                            live.apply_host_event(line);
                        }
                    }
                    row => live.apply_host_event(HostEvent::Row(row.clone())),
                }
            }
            let mut restored = App::new(launch_basic(), true);
            restored.theme = theme.clone();
            let mut rows = vec![header.clone()];
            rows.extend(rows_after_header.iter().cloned());
            restored.apply_host_event(restore_snapshot(1, rows));
            let paint = |app: &mut App| {
                let mut terminal = Terminal::new(TestBackend::new(width, 120)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                serialize_frame(terminal.backend().buffer())
            };
            let live_frame = paint(&mut live);
            assert_eq!(live_frame, paint(&mut restored), "width {width}");
            // The frame encodes a space as `·`. The process rows sit inside
            // the folded group; their text is pinned where rows map to blocks.
            if width == 120 {
                // Send times follow the process zone, so the painted label is
                // not a fixed UTC clock.
                let clock =
                    crate::tui::blocks::PromptClock::in_zone(live.time_zone, live.unix_now_ms());
                let user = clock.label(1_735_828_320_000).unwrap().replace(' ', "·");
                let steer = clock.label(1_735_828_380_000).unwrap().replace(' ', "·");
                assert!(live_frame.contains(&user), "{user} missing:\n{live_frame}");
                assert!(
                    live_frame.contains(&format!("steer·{steer}")),
                    "{steer} missing:\n{live_frame}"
                );
            }
        }
    }
}
