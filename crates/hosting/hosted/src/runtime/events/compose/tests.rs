#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::logging::project_presentation_snapshot;

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;

use crate::runtime::TurnProjection;
use crate::runtime::events::compose::{
    compose_cancelled_state, compose_counts_label, compose_program_source, emit_compose_update,
    observe_compose_subject, observe_settled_compose_marker,
};
use crate::runtime::events::tools::{
    observe_cancelled_tool_activity, observe_terminal_tool_activity, observe_tool_activity,
};
use crate::runtime::test_support::{host_event_sink, nested_lineage};
use crate::runtime::tree::continuation_parent_call_id;
use crate::sessions;
use crate::sessions::{SessionRecord, TranscriptRow};

/// The `wait` row sat flat between
/// children three and four of its cell. The host knows the cell's marker,
/// so the continuation folds under it — without being counted as a child.
/// Codex `exec` arguments are raw program
/// source carried as one JSON string; a Plak PTC body carries the program
/// under `source`. The Compose rail shows the program, never its encoding.
#[test]
fn a_compose_row_carries_its_program_source_as_detail() {
    assert_eq!(
        compose_program_source(Some(
            "// yield_time_ms=30000\n\nconst r = await tools.read_file({path: \"src/lib.rs\"});"
        ))
        .as_deref(),
        Some("// yield_time_ms=30000\n\nconst r = await tools.read_file({path: \"src/lib.rs\"});")
    );
    assert_eq!(
        compose_program_source(Some(
            r#""const spawned = await tools.spawn_agent({\n  class: \"review\"\n});""#
        ))
        .as_deref(),
        Some("const spawned = await tools.spawn_agent({\n  class: \"review\"\n});")
    );
    assert_eq!(
        compose_program_source(Some(
            r#"{"description":"add gcd and its test","source":"const x = 1;"}"#
        ))
        .as_deref(),
        Some("const x = 1;")
    );
    assert_eq!(
        compose_program_source(Some(r#"{"source":"\n# pragma\nprint(1)"}"#)).as_deref(),
        Some("# pragma\nprint(1)")
    );
    // Nothing to say stays nothing: no invented detail.
    assert_eq!(compose_program_source(None), None);
    assert_eq!(compose_program_source(Some("   \n\n")), None);

    // §5.8: the header is the host-counted tally alone.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose("exec-1", "exec", Some("cell-1"));
    projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
    assert_eq!(
        compose_counts_label(projection.cells.get("exec-1").expect("compose"), false),
        "1 tools"
    );
    projection.open_compose("exec-bare", "exec", Some("cell-bare"));
    assert_eq!(
        compose_counts_label(projection.cells.get("exec-bare").expect("compose"), false),
        "0 tools"
    );
}

#[test]
fn wait_continuation_folds_under_its_cell_marker_without_counting() {
    use plakous::agent::ToolCallerLineageView;

    let mut projection = TurnProjection::<TestExtension>::default();
    let wait = ToolCallerLineageView::local_composition_marker(Some("1"));
    assert_eq!(
        continuation_parent_call_id("wait", &wait, &projection),
        None,
        "a cell the host never opened stays a flat row"
    );
    projection.open_compose("exec-1", "exec", Some("1"));
    assert_eq!(
        continuation_parent_call_id("wait", &wait, &projection).as_deref(),
        Some("exec-1")
    );
    assert_eq!(
        continuation_parent_call_id("wait_js", &wait, &projection).as_deref(),
        Some("exec-1")
    );
    assert_eq!(
        continuation_parent_call_id("wait_py", &wait, &projection).as_deref(),
        Some("exec-1")
    );
    // Only a continuation folds: the marker itself and a plain tool do not.
    assert_eq!(
        continuation_parent_call_id("exec", &wait, &projection),
        None
    );
    assert_eq!(
        continuation_parent_call_id("read_file", &wait, &projection),
        None
    );
    let other_cell = ToolCallerLineageView::local_composition_marker(Some("2"));
    assert_eq!(
        continuation_parent_call_id("wait", &other_cell, &projection),
        None
    );
    assert_eq!(
        projection.cells["exec-1"].child_count(),
        0,
        "projecting a parent onto the row tallies nothing"
    );
}

/// A cell that yields before any nested call never named
/// its cell on a start event, so the `wait*` continuation was a flat row on
/// a broken rail and never re-terminalised its compose. The marker's own
/// terminal lineage carries the retired cell.
#[test]
fn a_cell_that_yields_before_any_child_learns_its_cell_from_the_marker_terminal() {
    use plakous::agent::ToolCallerLineageView;

    let mut projection = TurnProjection::<TestExtension>::default();
    // The interpreter had not minted the cell when the marker started.
    projection.open_compose("exec-1", "exec", None);
    let wait = ToolCallerLineageView::local_composition_marker(Some("cell-9"));
    assert_eq!(
        continuation_parent_call_id("wait", &wait, &projection),
        None,
        "nothing is guessed before the host has observed the ownership"
    );

    projection.learn_marker_cell("exec-1", Some("cell-9"));
    assert_eq!(
        projection.cells["exec-1"].cell_id.as_deref(),
        Some("cell-9")
    );
    assert_eq!(
        continuation_parent_call_id("wait", &wait, &projection).as_deref(),
        Some("exec-1"),
        "the wait continuation folds under the cell's own row"
    );
    // A cell the host already knows is never overwritten by a later one.
    projection.learn_marker_cell("exec-1", Some("cell-other"));
    assert_eq!(
        projection.cells["exec-1"].cell_id.as_deref(),
        Some("cell-9")
    );
}

/// A cell yielded, a child dispatched during the wait put the marker's
/// persisted row back to `running`, and nothing wrote it terminal again, so
/// the `Completed` sweep rewrote a turn that succeeded to
/// `failed:observation-gap` — live and on resume.
/// A Compose head's line carries the cell's persisted start and, once the
/// head is terminal, the finish its duration measures to: the stamps a
/// group's elapsed span reads live, as the restored row has them.
#[test]
fn a_compose_update_carries_the_cell_stamps() {
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.upsert_tool_activity(sessions::ToolItem {
        call_id: "exec-1".into(),
        name: "exec".into(),
        exposed_name: None,
        kind: crate::activity::tool_activity_kind("exec"),
        state: "running".into(),
        subject: None,
        detail: None,
        started_at_ms: Some(1_000),
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: Some("cell-1".into()),
        wait: None,
    });
    let (events, rx) = host_event_sink();
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose("exec-1", "exec", Some("cell-1"));
    projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
    let stamps =
        |rx: &crate::surface_protocol::HostEventReceiver<TestExtension>| match rx.try_iter().last()
        {
            Some(HostEvent::ToolLine {
                started_at_ms,
                finished_at_ms,
                ..
            }) => (started_at_ms, finished_at_ms),
            other => panic!("a compose line expected, got {other:?}"),
        };
    emit_compose_update(
        &events,
        &session,
        &projection.cells["exec-1"],
        "exec-1",
        false,
        None,
    );
    assert_eq!(stamps(&rx), (Some(1_000), None), "in flight");
    projection.observe_child_terminal("exec-1", Some("child-1"), false);
    projection.observe_compose_terminal("exec-1", "done");
    emit_compose_update(
        &events,
        &session,
        &projection.cells["exec-1"],
        "exec-1",
        false,
        Some(4_500),
    );
    assert_eq!(stamps(&rx), (Some(1_000), Some(5_500)), "terminal");
}

#[test]
fn a_reopened_compose_marker_re_terminalises_and_the_completed_sweep_skips_it() {
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let (events, rx) = host_event_sink();

    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose("exec-1", "exec", Some("cell-1"));
    observe_tool_activity(
        &mut session,
        sessions::ToolItem {
            call_id: "exec-1".into(),
            name: "exec".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("exec"),
            state: "running".into(),
            subject: None,
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: Some("cell-1".into()),
            wait: None,
        },
    );
    projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
    projection.observe_child_terminal("exec-1", Some("child-1"), false);
    // The cell yields with every observed child terminal, so its own
    // terminal paints and persists.
    projection.observe_compose_terminal("exec-1", "done");
    assert_eq!(
        projection.cells["exec-1"].settled_persisted_state(),
        Some("succeeded")
    );
    observe_settled_compose_marker(
        &mut session,
        &projection.cells["exec-1"],
        false,
        "exec-1",
        2,
    );

    // A child dispatched during the `wait*` puts the cell back in flight.
    assert!(projection.observe_child_start("exec-1", Some("child-2"), Some("cell-1"), Some(2)));
    assert_eq!(projection.cells["exec-1"].row_state(), "running");
    assert_eq!(
        projection.cells["exec-1"].settled_persisted_state(),
        None,
        "a cell with a running child has no terminal to persist"
    );
    let mut marker = session.transcript.tool_activity("exec-1").unwrap();
    "running".clone_into(&mut marker.state);
    marker.finished_at_ms = None;
    observe_tool_activity(&mut session, marker);

    // The wait-phase child closes: that is the cell's end.
    projection.observe_child_terminal("exec-1", Some("child-2"), false);
    observe_settled_compose_marker(
        &mut session,
        &projection.cells["exec-1"],
        false,
        "exec-1",
        9,
    );
    let settled = session.transcript.tool_activity("exec-1").unwrap();
    assert_eq!(settled.state, "succeeded");
    assert_eq!(settled.finished_at_ms, Some(9));
    assert_eq!(settled.subject.as_deref(), Some("2 tools"));

    // The `Completed` sweep rewrites persisted `running` rows only, so a
    // re-terminalised marker is not touched and emits no line.
    observe_terminal_tool_activity(
        &mut session,
        &events,
        "failed:observation-gap",
        Some(&projection),
    );
    let swept = session.transcript.tool_activity("exec-1").unwrap();
    assert_eq!(swept.state, "succeeded", "a cell that succeeded stays so");
    assert!(
        !rx.try_iter()
            .any(|event| matches!(event, HostEvent::ToolLine { .. })),
        "the sweep has nothing to rewrite"
    );
}

/// A cell that yields while a child is still in flight has
/// not ended, so its own terminal row stays `running` and a cancel in that
/// window reaches it — instead of leaving `✓ Compose` over `⊘` children.
#[test]
fn a_cancel_in_the_yield_window_sweeps_the_marker_and_states_the_remainder() {
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let (events, rx) = host_event_sink();

    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose("exec-1", "exec", Some("cell-1"));
    projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
    projection.observe_child_terminal("exec-1", Some("child-1"), false);
    projection.observe_child_start("exec-1", Some("child-2"), Some("cell-1"), Some(2));
    // The marker terminalises while child-2 runs: `row_state` is what the
    // line and the persisted row carry, not the local `done`.
    projection.observe_compose_terminal("exec-1", "done");
    assert_eq!(projection.cells["exec-1"].row_state(), "running");
    observe_tool_activity(
        &mut session,
        sessions::ToolItem {
            call_id: "exec-1".into(),
            name: "exec".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("exec"),
            state: "running".into(),
            subject: Some("2 tools".into()),
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: Some("cell-1".into()),
            wait: None,
        },
    );

    observe_cancelled_tool_activity(&mut session, &events, Some(&projection));
    let line = rx
        .try_iter()
        .find_map(|event| match event {
            HostEvent::ToolLine { call_id, state, .. } if call_id.as_deref() == Some("exec-1") => {
                Some(state)
            }
            _ => None,
        })
        .expect("the cancelled marker paints its own row");
    assert_eq!(line, "cancelled after 1 of 2");
}

/// The exec terminal persisted `3 tools`, two more children completed
/// under the `wait` continuation, the live row said `5 tools` and the
/// resumed one still `3 tools`.
#[test]
fn compose_counts_moved_by_wait_children_survive_resume() {
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    observe_tool_activity(
        &mut session,
        sessions::ToolItem {
            call_id: "exec-1".into(),
            name: "exec".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("exec"),
            state: "succeeded".into(),
            subject: Some("3 tools".into()),
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            parent_call_id: None,
            cell_id: Some("1".into()),
            wait: None,
        },
    );
    observe_compose_subject(&mut session, "exec-1", "5 tools".into());
    // A marker the host never persisted has nothing to move.
    observe_compose_subject(&mut session, "missing", "9 tools".into());

    let HostEvent::PresentationSnapshot(snapshot) =
        project_presentation_snapshot(&session.transcript)
    else {
        unreachable!("snapshot projection")
    };
    let tools: Vec<_> = snapshot
        .rows
        .iter()
        .filter_map(TranscriptRow::as_tool)
        .collect();
    let compose = tools
        .iter()
        .find(|item| item.call_id == "exec-1")
        .expect("the Compose head row is reloaded");
    assert_eq!(compose.subject.as_deref(), Some("5 tools"));
    assert_eq!(
        crate::activity::tool_activity_kind(&compose.name),
        plakous_host_protocol::views::presentation::ActivityKind::Compose
    );
    assert_eq!(compose.cell_id.as_deref(), Some("1"));
    assert_eq!(
        tools.len(),
        1,
        "re-persisting the tally rewrites the row rather than adding one"
    );
}

/// One nested cell's Compose row: created on the cell's own start, tallied on
/// each child terminal, never re-parented.
#[test]
fn compose_row_opens_on_the_cell_and_counts_observed_children() {
    let parent = plakous::model::CallId::try_from(uuid::Uuid::from_u128(0x0104)).unwrap();
    let parent_id = parent.as_uuid().to_string();
    for marker in ["exec", "exec_js", "exec_py"] {
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.open_compose(&parent_id, marker, Some("cell-1"));
        for ordinal in 1..=3_u32 {
            let child =
                plakous::model::CallId::try_from(uuid::Uuid::from_u128(u128::from(ordinal) + 1))
                    .unwrap();
            let lineage = nested_lineage("cell-1", ordinal, parent, child);
            projection.observe_child_start(
                &parent_id,
                Some(&child.as_uuid().to_string()),
                lineage.cell_id(),
                lineage.nested_ordinal(),
            );
        }
        for ordinal in 1..=3_u32 {
            let child =
                plakous::model::CallId::try_from(uuid::Uuid::from_u128(u128::from(ordinal) + 1))
                    .unwrap();
            projection.observe_child_terminal(
                &parent_id,
                Some(&child.as_uuid().to_string()),
                ordinal == 2,
            );
        }
        let compose = projection.cells.get(&parent_id).unwrap();
        assert_eq!(compose.name, marker);
        assert_eq!(compose.cell_id.as_deref(), Some("cell-1"));
        assert_eq!(compose_counts_label(compose, false), "3 tools · 1 failed");
        assert_eq!(
            compose_counts_label(compose, true),
            "3 tools · 1 failed · counts partial after gap"
        );
    }
}

/// A cell that cancels states only the remainder it observed.
#[test]
fn cancelled_compose_states_only_the_observed_remainder() {
    let parent = "outer-call";
    let mut projection = TurnProjection::<TestExtension>::default();

    // Nothing observed: no ratio may be invented.
    projection.open_compose(parent, "exec_js", Some("cell-1"));
    assert_eq!(
        compose_cancelled_state(projection.cells.get(parent).unwrap()),
        "cancelled"
    );

    // Four accepted, two reached a terminal state.
    for ordinal in 1..=4_u32 {
        projection.observe_child_start(
            parent,
            Some(&format!("child-{ordinal}")),
            Some("cell-1"),
            Some(ordinal),
        );
    }
    for ordinal in 1..=2_u32 {
        projection.observe_child_terminal(parent, Some(&format!("child-{ordinal}")), false);
    }
    assert_eq!(
        compose_cancelled_state(projection.cells.get(parent).unwrap()),
        "cancelled after 2 of 4"
    );
}

/// A child whose parent the host never observed is never folded or guessed.
#[test]
fn an_unknown_parent_never_mints_a_compose() {
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_child_start("never-seen", Some("child-1"), Some("cell-9"), Some(1));
    assert!(
        projection
            .observe_child_terminal("never-seen", Some("child-1"), false)
            .is_none()
    );
    assert!(projection.cells.is_empty());
}

/// A yielded Compose keeps `running` while any observed child is not
/// terminal, and paints its terminal only when they all are.
///
/// The outer `exec*` completes when the cell *yields*, and children
/// dispatched during the later `wait*` still carry the exec's
/// `parent_call_id`. Spec 02 §5.4 puts the running glyph on the compose and on
/// the child that is running, and §5.1 forbids inventing state: a `✓ Compose`
/// over `⊘ cancelled` children is exactly the lie those two clauses name.
#[test]
fn a_yielded_compose_stays_running_until_every_observed_child_is_terminal() {
    let marker = "outer-exec";
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose(marker, "exec_js", Some("cell-7"));
    assert!(!projection.observe_child_start(marker, Some("child-1"), Some("cell-7"), Some(1)));
    assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");

    // The cell yields with `child-1` still in flight: the marker has its own
    // observed terminal, but the row must not claim the cell is finished.
    projection.observe_compose_terminal(marker, "done");
    assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");

    // `child-1` reaches its terminal: every observed child is terminal, so
    // the marker's own observed state is what the row now paints.
    let compose = projection
        .observe_child_terminal(marker, Some("child-1"), false)
        .unwrap();
    assert_eq!(compose.row_state(), "done");
    assert_eq!(compose_counts_label(compose, false), "1 tools");

    // A second child dispatched during the `wait*` puts the cell back in
    // flight, and `observe_child_start` says so, so the caller re-paints the
    // marker row and persists it back to `running`.
    assert!(projection.observe_child_start(marker, Some("child-2"), Some("cell-7"), Some(2)));
    assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");

    // A `wait*` continuation names the cell and no parent; the host
    // attributes it to the row that cell already opened.
    assert_eq!(
        projection.marker_for_cell(Some("cell-7")).as_deref(),
        Some(marker)
    );
    assert!(projection.marker_for_cell(Some("cell-unknown")).is_none());
    assert!(projection.marker_for_cell(None).is_none());

    // The `wait*` terminal alone does not close a row with a live child.
    projection.observe_compose_terminal(marker, "failed");
    assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");
    let compose = projection
        .observe_child_terminal(marker, Some("child-2"), true)
        .unwrap();
    assert_eq!(compose.row_state(), "failed");
    assert_eq!(compose_counts_label(compose, false), "2 tools · 1 failed");
}
