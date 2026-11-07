use super::*;
use crate::tui::blocks::INTERRUPTED_MARKER;

fn all(_: usize) -> bool {
    true
}

#[test]
fn tool_rows_consume_the_host_operation_instead_of_reclassifying_the_display_name() {
    let mut item: plakous_host_protocol::views::transcript::ToolItem = serde_json::from_str(
        r#"{"call_id":"call","name":"unknown_alias","activity_kind":"tool","state":"done","subject":"7"}"#,
    ).unwrap();
    item.wire_operation = Some(plakous_host_protocol::views::ToolOperation::ReadProcess);
    let block = ScrollBlock::from_tool_item(item);
    assert_eq!(verb(&block), Some(ToolVerb::ReadProcess));
    let plans = plan(std::slice::from_ref(&block), RenderPrefs::default());
    let label = item_label(
        "unknown_alias",
        "done",
        ActivityKind::Tool,
        Some("7"),
        None,
        &plans[0],
    );
    assert_eq!(label, "Read output of process 7");
}

#[test]
fn process_actions_name_the_operation_and_outcome() {
    for (name, state, subject, expected) in [
        ("Shell", "running", "cargo test", "  ⠼ $ cargo test"),
        (
            "process_session_poll",
            "running",
            "7 · cargo test",
            "  ⠼ Waiting on process 7 · cargo test",
        ),
        (
            "process_session_poll",
            "done",
            "7 · cargo test · exited 0",
            "  ✓ Waited on process 7 · cargo test · exited 0",
        ),
        (
            "process_session_poll",
            "done",
            "7 · cargo test · still running",
            "  ✓ Waited on process 7 · cargo test · still running",
        ),
        (
            "process_session_poll",
            "failed:exit 7",
            "7 · cargo test · exited 7",
            "  ✗ Waited on process 7 · cargo test · exited 7",
        ),
        (
            "process_session_poll",
            "done",
            "Read output of process 7 · cargo test",
            "  ✓ Read output of process 7 · cargo test",
        ),
        (
            "process_session_write",
            "done",
            "7 · 4 bytes",
            "  ✓ Sent input to process 7 · 4 bytes",
        ),
        (
            "process_session_terminate",
            "done",
            "7 · cargo test",
            "  ✓ Stopped process 7 · cargo test",
        ),
        (
            "process_session_poll",
            "running",
            "7",
            "  ⠼ Waiting on process 7",
        ),
    ] {
        assert_eq!(
            action(
                name,
                state,
                ActivityKind::Process,
                Some(subject),
                &RowPlan::default()
            ),
            expected
        );
    }
}

#[test]
fn process_interrupt_keeps_its_action_label() {
    assert_eq!(
        action(
            "process_session_write",
            "done",
            ActivityKind::Process,
            Some("Sent interrupt to process 7 · cargo test"),
            &RowPlan::default()
        ),
        "  ✓ Sent interrupt to process 7 · cargo test"
    );
}

#[test]
fn process_operations_have_their_own_summary_nouns() {
    let blocks = [
        tool(
            "process_session_poll",
            "done",
            "Waited on process 7 · cargo test · still running",
        ),
        tool(
            "process_session_write",
            "done",
            "Sent input to process 7 · 4 bytes",
        ),
        tool(
            "process_session_terminate",
            "done",
            "Stopped process 7 · cargo test",
        ),
    ];
    let found = summary(&blocks, &all);
    assert_eq!(
        found.label,
        "Checked 1 process, sent input to 1 process, stopped 1 process"
    );
    assert!(!found.failed);
}

#[test]
fn process_rows_never_have_an_empty_command_label() {
    for name in [
        "Shell",
        "exec_command",
        "Bash",
        "BashOutput",
        "TaskStop",
        "KillShell",
        "write_stdin",
        "run_terminal_command",
        "get_command_or_subagent_output",
        "process_session_start",
        "process_session_write",
        "process_session_poll",
        "process_session_terminate",
    ] {
        for state in ["running", "done"] {
            let label = action(
                name,
                state,
                ActivityKind::Process,
                None,
                &RowPlan::default(),
            );
            assert!(!label.trim().is_empty(), "{name}: {label}");
            assert!(!label.ends_with("$ "), "{name}: {label}");
        }
    }
}

fn tool(name: &str, state: &str, subject: &str) -> ScrollBlock {
    ScrollBlock::Tool {
        call_id: None,
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        name: name.into(),
        state: state.into(),
        subject: Some(subject.into()),
        detail: None,
        mode: DisplayMode::Collapsed,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: crate::tui::blocks::ItemView::default(),
    }
}

#[test]
fn unknown_edit_totals_are_omitted() {
    let block = tool("Edit", "done", "foo+3.rs");
    let plans = plan(std::slice::from_ref(&block), RenderPrefs::default());
    assert_eq!(
        action(
            "Edit",
            "done",
            ActivityKind::Tool,
            Some("foo+3.rs"),
            &plans[0]
        ),
        "  ✓ Edited foo+3.rs"
    );
    assert_eq!(summary(&[block], &all).label, "Edited 1 file");
    assert_eq!(
        action(
            "Edit",
            "done",
            ActivityKind::Tool,
            Some("foo +0/-0"),
            &RowPlan::default()
        ),
        "  ✓ Edited foo"
    );
}

#[test]
fn topology_operations_and_failed_spawns_never_claim_finished_agents() {
    for name in [
        "close_agent",
        "interrupt_agent",
        "recover_agent",
        "discard_workspace",
        "stop_all",
        "submit_result",
        "spawn_agent",
    ] {
        let mut block = tool(name, "failed: refused", "child");
        if let ScrollBlock::Tool { kind, .. } = &mut block {
            *kind = ActivityKind::Agent;
        }
        assert!(
            !summary(&[block], &all).label.contains("finished"),
            "{name}"
        );
        assert!(
            !action(
                name,
                "failed: refused",
                ActivityKind::Agent,
                Some("child"),
                &RowPlan::default()
            )
            .contains("finished"),
            "{name}"
        );
    }
}

#[test]
fn folded_groups_keep_resolved_approvals_and_errors_end_groups() {
    let blocks = [
        tool("read_file", "done", "a"),
        tool("grep", "done", "b"),
        ScrollBlock::Approval {
            approval_id: "a".into(),
            tool_name: "Edit".into(),
            subject: None,
            prompt: None,
            resolution: Some("approved".into()),
        },
        ScrollBlock::Assistant("answer".into()),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert!(plans[0].group_folded);
    assert!(plans[1].hidden);
    assert!(!plans[2].hidden);
    let blocks = [
        tool("read_file", "done", "a"),
        ScrollBlock::Error("failure".into()),
        tool("grep", "done", "b"),
        ScrollBlock::Assistant("answer".into()),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert!(plans.iter().all(|plan| !plan.hidden));
    assert!(plans.iter().all(|plan| plan.activity_summary.is_none()));
}

#[test]
fn interrupted_marker_does_not_leave_a_dangling_group_rail() {
    let mut first = tool("read_file", "interrupted", "a");
    if let ScrollBlock::Tool { group_open, .. } = &mut first {
        *group_open = true;
    }
    let blocks = [
        first,
        ScrollBlock::System(INTERRUPTED_MARKER.into()),
        ScrollBlock::Assistant("answer".into()),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert_eq!(plans[0].group_child, Some(true));
    assert!(!plans[1].hidden);
}

#[test]
fn first_expanded_member_has_a_rail() {
    let mut first = tool("read_file", "done", "a");
    if let ScrollBlock::Tool { group_open, .. } = &mut first {
        *group_open = true;
    }
    let blocks = [
        first,
        tool("Edit", "done", "b"),
        ScrollBlock::Assistant("answer".into()),
    ];
    assert_eq!(
        plan(&blocks, RenderPrefs::default())[0].group_child,
        Some(false)
    );
}

#[test]
fn diffstat_requires_a_complete_trailing_suffix() {
    for subject in [
        "foo-2.rs",
        "foo+3.rs",
        "foo +3",
        "foo -2",
        "foo +3/-2 extra",
        "foo +3x/-2",
    ] {
        assert_eq!(edit_counts(subject), None, "{subject}");
    }
    assert_eq!(edit_counts("foo-2.rs +12/-3"), Some((12, 3)));
}

#[test]
fn exploration_omits_zero_components() {
    let blocks = [
        tool("read_file", "done", "a.rs"),
        tool("read_file", "done", "b.rs"),
    ];
    let prefs = RenderPrefs {
        group_tool_calls: false,
        ..RenderPrefs::default()
    };
    assert_eq!(
        plan(&blocks, prefs)[0].activity_action.as_deref(),
        Some("Explored 2 files")
    );
}

#[test]
fn historic_spawns_do_not_animate_and_finished_agents_are_unique() {
    let blocks = [
        tool("spawn_agent", "done", "child"),
        ScrollBlock::Assistant("Done".into()),
    ];
    let prefs = RenderPrefs {
        group_tool_calls: false,
        ..RenderPrefs::default()
    };
    let plans = plan(&blocks, prefs);
    assert!(!plans[0].activity_running);
    assert_eq!(plans[0].activity_action.as_deref(), Some("1 agent spawned"));
    let blocks = [
        tool("wait_agent", "done", "a,b"),
        tool("wait_agent", "done", "b"),
    ];
    assert_eq!(
        plan(&blocks, prefs)[0].activity_action.as_deref(),
        Some("2 agents finished")
    );
    let blocks = [
        tool("spawn_agent", "done", "child"),
        tool("wait_agent", "done", "a"),
    ];
    let plans = plan(&blocks, prefs);
    assert!(!plans[0].activity_running);
    assert_eq!(plans[0].activity_action.as_deref(), Some("1 agent spawned"));
}

#[test]
fn large_group_keeps_one_head_after_front_eviction() {
    let mut blocks: Vec<_> = (0..600)
        .flat_map(|n| {
            [
                tool("read_file", "done", &format!("file-{n}.rs")),
                ScrollBlock::Discarded {
                    text: "thought".into(),
                    thinking: true,
                },
            ]
        })
        .collect();
    blocks.push(ScrollBlock::Assistant("Answer".into()));
    let plans = plan(&blocks, RenderPrefs::default());
    assert_eq!(plans[0].activity_summary.as_deref(), Some("Read 600 files"));
    blocks.drain(..200);
    let plans = plan(&blocks, RenderPrefs::default());
    assert_eq!(plans[0].activity_summary.as_deref(), Some("Read 500 files"));
    assert_eq!(
        plans
            .iter()
            .filter(|p| p.activity_summary.is_some())
            .count(),
        1
    );
}

#[test]
fn failed_exits_are_unique_and_bounded() {
    let blocks: Vec<_> = [1, 2, 3, 4, 1, 5]
        .map(|n| tool("exec_command", &format!("failed:exit {n}"), "false"))
        .into();
    let found = summary(&blocks, &all);
    assert!(found.failed);
    assert_eq!(found.label, "Ran 6 commands (6 failed)");
    assert_eq!(found.meta.as_deref(), Some("exit 1, exit 2, exit 3, +2"));
}

fn running(name: &str, id: &str, subject: &str) -> ScrollBlock {
    let mut block = tool(name, "running", subject);
    if let ScrollBlock::Tool { call_id, .. } = &mut block {
        *call_id = Some(id.into());
    }
    block
}

#[test]
fn live_group_is_a_summary_of_finished_calls_plus_one_tip_per_running_call() {
    let tips_only = [
        running("exec_command", "a", "cargo test"),
        running("exec_command", "b", "cargo build"),
    ];
    let plans = plan(&tips_only, RenderPrefs::default());
    assert!(plans.iter().all(|plan| plan.activity_summary.is_none()));
    assert!(plans.iter().all(|plan| !plan.hidden));

    let blocks = [
        tool("read_file", "done:1.0s", "a.rs"),
        running("exec_command", "a", "cargo test"),
        tool("grep", "done:0.5s", "needle"),
        running("exec_command", "b", "cargo build"),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert_eq!(
        plans[0].activity_summary.as_deref(),
        Some("Read 1 file, ran 1 search")
    );
    assert_eq!(plans[0].summary_meta.as_deref(), Some("1.5s"));
    assert!(
        plans[0].group_folded,
        "a finished head draws only the summary"
    );
    assert!(!plans[0].hidden);
    assert!(
        !plans[1].hidden && !plans[3].hidden,
        "one tip per running call"
    );
    assert!(plans[2].hidden, "a finished call folds into the summary");
}

#[test]
fn settled_group_folds_to_one_line_and_a_single_call_keeps_its_item() {
    let one = [
        tool("exec_command", "done:2.0s", "cargo test"),
        ScrollBlock::Assistant("ok".into()),
    ];
    let plans = plan(&one, RenderPrefs::default());
    assert!(plans[0].activity_summary.is_none());
    assert!(!plans[0].group_folded);

    let many = [
        tool("read_file", "done:30.0s", "a.rs"),
        tool("exec_command", "failed:exit 2 · 32.0s", "cargo test"),
        ScrollBlock::item("retry 2/4 · rate limited"),
        ScrollBlock::Assistant("ok".into()),
    ];
    let plans = plan(&many, RenderPrefs::default());
    assert_eq!(
        plans[0].activity_summary.as_deref(),
        Some("Read 1 file, ran 1 command (1 failed)")
    );
    assert_eq!(plans[0].summary_meta.as_deref(), Some("exit 2 · 1m02s"));
    assert!(plans[0].group_folded);
    assert!(plans[1].hidden && plans[2].hidden);

    let ungrouped = plan(
        &many,
        RenderPrefs {
            group_tool_calls: false,
            ..RenderPrefs::default()
        },
    );
    assert!(ungrouped.iter().all(|plan| !plan.hidden));
    assert!(ungrouped.iter().all(|plan| plan.activity_summary.is_none()));
}

#[test]
fn summary_nouns_name_every_operation_and_never_say_actions() {
    let blocks = [
        tool("read_file", "done", "a"),
        tool("grep", "done", "b"),
        tool("apply_patch", "done", "c +1/-2"),
        tool("write_file", "done", "d"),
        tool("exec_command", "done", "ls"),
        tool("process_session_poll", "done", "Waited on process 7 · x"),
        tool("process_session_terminate", "done", "Stopped process 7 · x"),
        tool("send_agent_message", "done", "c23 · hi"),
        tool("list_agents", "done", ""),
        tool("close_agent", "done", "c1"),
        tool("update_plan", "done", "3/9 done"),
        tool("web_search", "done", "q"),
        tool("web_search", "done", "r"),
    ];
    let label = summary(&blocks, &all).label;
    assert_eq!(
        label,
        "Read 1 file, ran 1 search, edited 1 file (+1 −2), wrote 1 file, ran 1 command, \
             checked 1 process, stopped 1 process, messaged 1 agent, listed agents, \
             closed 1 agent, updated plan, web search ×2"
    );
    assert!(!label.contains("action"));
    let checks = [
        tool("process_session_poll", "done", "Waited on process 7 · x"),
        tool(
            "process_session_poll",
            "done",
            "Read output of process 7 · x",
        ),
    ];
    assert_eq!(summary(&checks, &all).label, "Checked 2 processes");
}

#[test]
fn shown_thinking_counts_in_a_group_and_hidden_thinking_does_not() {
    let thinking = ScrollBlock::Thinking {
        location_key: "r".into(),
        text: "hmm".into(),
        sealed: true,
        elapsed_ms: None,
        mode: DisplayMode::Collapsed,
        manual: false,
    };
    let blocks = [
        tool("read_file", "done", "a"),
        tool("read_file", "done", "b"),
        thinking,
        tool("read_file", "done", "c"),
        tool("read_file", "done", "d"),
        ScrollBlock::Assistant("ok".into()),
    ];
    let shown = plan(&blocks, RenderPrefs::default());
    assert_eq!(
        shown[0].activity_summary.as_deref(),
        Some("Read 4 files, 1 thought")
    );
    assert!(shown[3].activity_summary.is_none());
    let hidden = plan(
        &blocks,
        RenderPrefs {
            show_thinking: false,
            ..RenderPrefs::default()
        },
    );
    assert_eq!(hidden[0].activity_summary.as_deref(), Some("Read 4 files"));
    assert!(hidden[3].activity_summary.is_none());
}

fn sealed_thought(key: &str) -> ScrollBlock {
    ScrollBlock::Thinking {
        location_key: key.into(),
        text: "hmm".into(),
        sealed: true,
        elapsed_ms: Some(1_200),
        mode: DisplayMode::Collapsed,
        manual: false,
    }
}

fn grouped() -> RenderPrefs {
    RenderPrefs::default()
}

#[test]
fn thinking_between_calls_stays_inside_the_group_and_is_counted() {
    let blocks = [
        tool("read_file", "done", "a"),
        sealed_thought("r1"),
        tool("read_file", "done", "b"),
        sealed_thought("r2"),
        tool("exec_command", "done", "cargo test"),
        sealed_thought("r3"),
        ScrollBlock::Assistant("ok".into()),
    ];
    let plans = plan(&blocks, grouped());
    assert_eq!(
        plans[0].activity_summary.as_deref(),
        Some("Read 2 files, ran 1 command, 2 thoughts")
    );
    assert!(plans[0].group_folded);
    assert!(
        (1..5).all(|index| plans[index].hidden),
        "the fold covers the thoughts between its calls"
    );
    assert!(
        !plans[5].hidden && plans[5].group_child.is_none(),
        "a thought after the last call is not inside the group"
    );

    // Opening the group shows every member, thoughts included, in order.
    let mut open = blocks.clone();
    if let ScrollBlock::Tool { group_open, .. } = &mut open[0] {
        *group_open = true;
    }
    let plans = plan(&open, grouped());
    assert!((0..5).all(|index| !plans[index].hidden));
    assert_eq!(plans[1].group_child, Some(false));
    assert_eq!(plans[3].group_child, Some(false));
    assert_eq!(plans[4].group_child, Some(true));

    // Hidden thinking is never counted.
    let hidden = plan(
        &blocks,
        RenderPrefs {
            show_thinking: false,
            ..grouped()
        },
    );
    assert_eq!(
        hidden[0].activity_summary.as_deref(),
        Some("Read 2 files, ran 1 command")
    );
}

#[test]
fn a_thought_joins_the_group_once_the_next_call_arrives() {
    let live = [tool("read_file", "done", "a"), sealed_thought("r1")];
    let plans = plan(&live, grouped());
    assert!(
        !plans[1].hidden,
        "a thought with no later call stays outside"
    );
    assert!(plans[0].activity_summary.is_none());
    let next = [
        tool("read_file", "done", "a"),
        sealed_thought("r1"),
        tool("read_file", "running", "b"),
    ];
    let plans = plan(&next, grouped());
    assert_eq!(
        plans[0].activity_summary.as_deref(),
        Some("Read 1 file, 1 thought")
    );
    assert!(plans[1].hidden);
}

#[test]
fn a_thought_in_an_open_group_sits_on_the_group_rail() {
    let mut blocks = vec![
        tool("read_file", "done", "a"),
        sealed_thought("r1"),
        tool("read_file", "done", "b"),
        ScrollBlock::Assistant("ok".into()),
    ];
    if let ScrollBlock::Tool { group_open, .. } = &mut blocks[0] {
        *group_open = true;
    }
    let rows = crate::tui::blocks::layout_blocks_with(&blocks, 80, grouped()).plain;
    let at = rows
        .iter()
        .position(|row| row.starts_with("  ├─  ◆ hmm"))
        .unwrap_or_else(|| panic!("{rows:#?}"));
    assert!(rows[at + 1].starts_with("  └─  "), "{rows:#?}");
}

#[test]
fn code_mode_cells_read_code_mode_in_a_group_summary() {
    let blocks = [
        tool("read_file", "done", "a"),
        cell("c1", "done:1.0s", DisplayMode::Collapsed),
        cell("c2", "done:2.0s", DisplayMode::Collapsed),
    ];
    assert_eq!(
        summary(&blocks, &all).label,
        "Read 1 file, CodeMode ×2 · 6 calls"
    );
    assert_eq!(summary(&blocks[1..2], &all).label, "CodeMode · 3 calls");
}

#[test]
fn review_trailing_empty_thought_has_no_row_and_no_count() {
    let mut empty = sealed_thought("trailing");
    if let ScrollBlock::Thinking { text, .. } = &mut empty {
        text.clear();
    }
    let blocks = [
        tool("read_file", "done", "a"),
        tool("read_file", "done", "b"),
        empty,
        ScrollBlock::Assistant("Done".into()),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert_eq!(plans[0].activity_summary.as_deref(), Some("Read 2 files"));
    assert!(plans[2].hidden);
}

#[test]
fn review_lone_calls_keep_thought_rows_without_a_summary() {
    for state in ["running", "done"] {
        for empty in [false, true] {
            let mut thought = sealed_thought("lead");
            if empty && let ScrollBlock::Thinking { text, .. } = &mut thought {
                text.clear();
            }
            for call in [
                tool("read_file", state, "a.rs"),
                cell("c", state, DisplayMode::Collapsed),
            ] {
                let mut blocks = vec![thought.clone(), call];
                if matches!(
                    blocks[1],
                    ScrollBlock::Tool {
                        kind: ActivityKind::Compose,
                        ..
                    }
                ) {
                    blocks.push(nested("c", "exec_command", state, "pwd"));
                }
                let plans = plan(&blocks, RenderPrefs::default());
                assert!(plans.iter().all(|p| p.activity_summary.is_none()));
                assert_eq!(plans[0].hidden, empty);
                assert!(!plans[1].hidden);
                if blocks.len() > 2 {
                    assert!(!plans[2].hidden);
                }
            }
        }
    }
}

#[test]
fn review_leading_thought_stays_at_tip_until_a_summary_exists() {
    let mut blocks = vec![sealed_thought("lead"), tool("read_file", "running", "a")];
    assert!(!plan(&blocks, RenderPrefs::default())[0].hidden);
    blocks.push(tool("exec_command", "running", "pwd"));
    assert!(!plan(&blocks, RenderPrefs::default())[0].hidden);
    if let ScrollBlock::Tool { state, .. } = &mut blocks[1] {
        *state = "done".into();
    }
    assert_eq!(
        plan(&blocks, RenderPrefs::default())[0]
            .activity_summary
            .as_deref(),
        Some("Read 1 file, 1 thought")
    );
}

#[test]
fn review_nested_cells_and_invalid_lineage_keep_every_row() {
    for parents in [
        [None, Some("a"), Some("b")],
        [Some("a"), None, Some("b")],
        [Some("b"), Some("a"), Some("b")],
    ] {
        let mut blocks = [
            cell("a", "done", DisplayMode::Expanded),
            cell("b", "done", DisplayMode::Expanded),
            nested("b", "exec_command", "done", "grandchild"),
        ];
        for (block, parent) in blocks.iter_mut().zip(parents) {
            if let ScrollBlock::Tool { parent_call_id, .. } = block {
                *parent_call_id = parent.map(str::to_owned);
            }
        }
        let prefs = RenderPrefs {
            group_tool_calls: false,
            ..RenderPrefs::default()
        };
        let plans = plan(&blocks, prefs);
        let order = crate::tui::blocks::row_order(&plans);
        assert_eq!(order.len(), 3);
        assert_eq!(
            order.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        assert!(
            crate::tui::blocks::layout_blocks_with(&blocks, 80, prefs)
                .plain
                .iter()
                .any(|r| r.contains("grandchild"))
        );
    }
}

#[test]
fn leading_thoughts_fold_and_empty_thoughts_never_split_groups() {
    let mut empty = sealed_thought("empty");
    if let ScrollBlock::Thinking { text, .. } = &mut empty {
        text.clear();
    }
    let blocks = [
        sealed_thought("lead"),
        tool("read_file", "done", "a"),
        empty,
        tool("exec_command", "done", "pwd"),
        ScrollBlock::Assistant("ok".into()),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert_eq!(
        plans[0].activity_summary.as_deref(),
        Some("Read 1 file, ran 1 command, 2 thoughts")
    );
    assert!((1..4).all(|i| plans[i].hidden));
    let mut alone = sealed_thought("alone");
    if let ScrollBlock::Thinking { text, .. } = &mut alone {
        text.clear();
    }
    assert_eq!(
        crate::tui::blocks::layout_blocks(&[alone], 80).plain,
        [] as [String; 0]
    );
}

#[test]
fn collapsed_cell_outline_is_capped_and_prioritizes_running_and_failed_calls() {
    for count in [0, 1, 10, 11, 50] {
        let mut head = cell("c", "done:4.2s", DisplayMode::Collapsed);
        if let ScrollBlock::Tool { subject, .. } = &mut head {
            *subject = Some(format!("{count} tools"));
        }
        let mut blocks = vec![head];
        for i in 0..count {
            let state = if i == 48 {
                "running"
            } else if i == 49 {
                "failed:exit 2"
            } else {
                "done"
            };
            blocks.push(nested("c", "exec_command", state, &format!("command-{i}")));
        }
        let plans = plan(&blocks, RenderPrefs::default());
        assert_eq!(
            plans.iter().skip(1).filter(|p| !p.hidden).count(),
            count.min(10)
        );
        let rows = crate::tui::blocks::layout_blocks(&blocks, 100)
            .plain
            .join("\n");
        if count == 0 {
            assert!(
                rows.contains("CodeMode") && !rows.contains("0 calls"),
                "{rows}"
            );
        }
        if count > 10 {
            assert!(rows.contains(&format!("… {} more", count - 10)), "{rows}");
        }
        if count == 50 {
            assert!(
                rows.contains("command-48") && rows.contains("command-49"),
                "{rows}"
            );
            assert!(rows.contains("(1 failed)"), "{rows}");
            assert!(rows.starts_with("  ⠼"), "{rows}");
        }
    }
}

fn cell(id: &str, state: &str, mode: DisplayMode) -> ScrollBlock {
    ScrollBlock::Tool {
        call_id: Some(id.into()),
        parent_call_id: None,
        cell_id: Some(id.into()),
        kind: ActivityKind::Compose,
        name: "exec".into(),
        state: state.into(),
        subject: Some("3 tools".into()),
        detail: Some("await tools.exec_command()".into()),
        mode,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: crate::tui::blocks::ItemView::default(),
    }
}

fn nested(parent: &str, name: &str, state: &str, subject: &str) -> ScrollBlock {
    let mut block = tool(name, state, subject);
    if let ScrollBlock::Tool { parent_call_id, .. } = &mut block {
        *parent_call_id = Some(parent.into());
    }
    block
}

#[test]
fn a_running_cell_tip_keeps_its_tally_and_outline() {
    let blocks = [
        cell("c", "running", DisplayMode::Collapsed),
        nested("c", "read_file", "done", "a.rs"),
        nested("c", "exec_command", "running", "cargo test"),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert_eq!(
        plans[0].activity_action.as_deref(),
        Some("CodeMode · 3 calls")
    );
    assert!(!plans[1].hidden && !plans[2].hidden);
    let done = [
        cell("c", "done:4.2s", DisplayMode::Collapsed),
        nested("c", "read_file", "done", "a.rs"),
        ScrollBlock::Assistant("ok".into()),
    ];
    let plans = plan(&done, RenderPrefs::default());
    assert_eq!(
        plans[0].activity_action.as_deref(),
        Some("CodeMode · 3 calls")
    );
    assert_eq!(
        item_meta(
            "exec",
            "done:4.2s",
            ActivityKind::Compose,
            Some("3 tools"),
            &plans[0]
        )
        .as_deref(),
        Some("4.2s")
    );
}

#[test]
fn opening_a_group_does_not_open_its_cells_and_nested_calls_sit_one_rail_deeper() {
    let mut head = tool("read_file", "done", "a");
    if let ScrollBlock::Tool { group_open, .. } = &mut head {
        *group_open = true;
    }
    let blocks = [
        head.clone(),
        cell("c", "done", DisplayMode::Collapsed),
        nested("c", "read_file", "done", "b"),
        ScrollBlock::Assistant("ok".into()),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert!(!plans[2].hidden, "a collapsed cell keeps its outline");
    let blocks = [
        head,
        cell("c", "done", DisplayMode::Expanded),
        nested("c", "read_file", "done", "b"),
        tool("grep", "done", "x"),
        ScrollBlock::Assistant("ok".into()),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert!(!plans[2].hidden);
    assert_eq!(row_indent(&plans[1]), "  ├─  ");
    assert_eq!(row_indent(&plans[2]), "  │   └─  ");
    assert_eq!(row_indent(&plans[3]), "  └─  ");
}

#[test]
fn opened_cell_group_keeps_each_cells_own_failure_glyph() {
    let mut first = cell("a", "done", DisplayMode::Collapsed);
    if let ScrollBlock::Tool { group_open, .. } = &mut first {
        *group_open = true;
    }
    let blocks = [
        first,
        cell("b", "done", DisplayMode::Collapsed),
        nested("b", "exec_command", "failed:exit 2", "false"),
    ];
    let plans = plan(&blocks, RenderPrefs::default());
    assert_eq!(item_glyph("done", ActivityKind::Compose, &plans[0]), "✓");
    assert_eq!(item_glyph("done", ActivityKind::Compose, &plans[1]), "✗");
    assert!(
        plans[0].activity_failed,
        "group summary still reports the failure"
    );
}

#[test]
fn interleaved_cell_children_render_in_each_outline_in_call_order() {
    let blocks = [
        cell("a", "done", DisplayMode::Collapsed),
        cell("b", "done", DisplayMode::Collapsed),
        nested("a", "exec_command", "done", "a-first"),
        nested("b", "exec_command", "done", "b-first"),
        nested("a", "exec_command", "done", "a-last"),
    ];
    let prefs = RenderPrefs {
        group_tool_calls: false,
        ..RenderPrefs::default()
    };
    let rows = crate::tui::blocks::layout_blocks_with(&blocks, 80, prefs).plain;
    assert!(
        rows[1].contains("a-first") && rows[2].contains("a-last"),
        "{rows:?}"
    );
    assert!(rows[4].contains("b-first"), "{rows:?}");
}

#[test]
fn two_cells_summary_totals_host_calls_and_live_outline_folds_when_done() {
    let mut second = cell("b", "running", DisplayMode::Collapsed);
    if let ScrollBlock::Tool { subject, .. } = &mut second {
        *subject = Some("4 tools".into());
    }
    let mut blocks = [
        cell("a", "done", DisplayMode::Collapsed),
        nested("a", "read_file", "done", "a.rs"),
        second,
        nested("b", "exec_command", "running", "pwd"),
    ];
    let live = plan(&blocks, RenderPrefs::default());
    assert!(live[1].hidden && !live[2].hidden && !live[3].hidden);
    if let ScrollBlock::Tool { state, .. } = &mut blocks[2] {
        *state = "done".into();
    }
    if let ScrollBlock::Tool { state, .. } = &mut blocks[3] {
        *state = "done".into();
    }
    let done = plan(&blocks, RenderPrefs::default());
    assert_eq!(
        done[0].activity_summary.as_deref(),
        Some("CodeMode ×2 · 7 calls")
    );
    assert!((1..4).all(|i| done[i].hidden));
}

#[test]
fn streaming_leading_thought_is_pulled_in_only_after_sealing() {
    let mut thought = sealed_thought("t");
    if let ScrollBlock::Thinking { sealed, .. } = &mut thought {
        *sealed = false;
    }
    let mut blocks = [
        tool("read_file", "done", "a"),
        thought,
        tool("read_file", "done", "b"),
    ];
    let streaming = plan(&blocks, RenderPrefs::default());
    assert!(!streaming[1].hidden && streaming[0].activity_summary.is_none());
    if let ScrollBlock::Thinking { sealed, .. } = &mut blocks[1] {
        *sealed = true;
    }
    let sealed = plan(&blocks, RenderPrefs::default());
    assert_eq!(
        sealed[0].activity_summary.as_deref(),
        Some("Read 2 files, 1 thought")
    );
    assert!(sealed[1].hidden);
}

#[test]
fn item_text_is_one_line_with_exit_and_duration_as_meta() {
    let plan = RowPlan::default();
    assert_eq!(
        action(
            "exec_command",
            "failed:exit 2 · 1.5s",
            ActivityKind::Process,
            Some("echo a\necho b"),
            &plan
        ),
        "  ✗ $ echo a↵echo b"
    );
    assert_eq!(
        item_meta(
            "exec_command",
            "failed:exit 2 · 1.5s",
            ActivityKind::Process,
            Some("echo a"),
            &plan
        )
        .as_deref(),
        Some("exit 2 · 1.5s")
    );
    assert_eq!(
        action(
            "exec_command",
            "done:1.0s",
            ActivityKind::Process,
            Some("ls"),
            &plan
        ),
        "  ✓ $ ls"
    );
}

/// Parallel calls overlap: the meta is the span from the first start to
/// the last finish, not the sum; one call without host stamps falls the
/// whole group back to the sum.
#[test]
fn group_meta_is_the_stamped_span_and_falls_back_to_the_sum() {
    let stamped = |subject: &str, state: &str, stamps: Option<(u64, u64)>| {
        let mut block = tool("exec_command", state, subject);
        if let ScrollBlock::Tool {
            started_at_ms,
            finished_at_ms,
            ..
        } = &mut block
        {
            *started_at_ms = stamps.map(|(start, _)| start);
            *finished_at_ms = stamps.map(|(_, finish)| finish);
        }
        block
    };
    let all = |_| true;
    let parallel = [
        stamped("a", "done:5.0s", Some((10_000, 15_000))),
        stamped("b", "done:3.0s", Some((11_000, 14_000))),
    ];
    assert_eq!(summary(&parallel, &all).meta.as_deref(), Some("5.0s"));
    let carried = [
        stamped("a", "done:5.0s", Some((10_000, 15_000))),
        stamped("b", "done:3.0s", None),
    ];
    assert_eq!(summary(&carried, &all).meta.as_deref(), Some("8.0s"));
}

#[test]
fn group_and_tip_durations_format_for_the_reader() {
    assert_eq!(format_group_elapsed(62_000), "1m02s");
    assert_eq!(format_group_elapsed(3_900_000), "1h05m");
    assert_eq!(format_group_elapsed(1_500), "1.5s");
    assert_eq!(elapsed_ms("12ms"), Some(12));
    assert_eq!(elapsed_ms("<1ms"), Some(0));
    assert_eq!(elapsed_ms("2.5s"), Some(2_500));
    let tip = |secs| tip_elapsed(std::time::Duration::from_secs(secs));
    assert_eq!(tip(0), None);
    assert_eq!(tip(9).as_deref(), Some("9s"));
    assert_eq!(tip(75).as_deref(), Some("1m15s"));
    assert_eq!(tip(65).as_deref(), Some("1m05s"));
    assert_eq!(tip(3_900).as_deref(), Some("1h05m"));
}
