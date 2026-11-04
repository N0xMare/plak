use super::*;

const NOW: u64 = 10_000;

fn layout_node(id: &str, parent: Option<&str>) -> LayoutNode {
    LayoutNode {
        id: id.into(),
        parent: parent.map(str::to_owned),
        started_at_unix: None,
    }
}

fn node(id: &str, parent: Option<&str>, status: &str) -> CanvasNode {
    CanvasNode {
        agent_id: id.into(),
        parent_agent_id: parent.map(str::to_owned),
        depth: u8::from(parent.is_some()),
        status: status.into(),
        attempt_status: None,
        attempt_seq: parent.map(|_| 1),
        attempt_id: parent.map(|_| format!("{id}-a1")),
        attempts: Vec::new(),
        tier: None,
        description: "d".into(),
        prompt: "p".into(),
        workspace_path: "/w".into(),
        workspace_mode: "inherit".into(),
        workspace_status: None,
        candidate_id: None,
        blocked: None,
        usage: None,
        active_turn: false,
        live_progress: None,
        unread_mail: 0,
        has_session: true,
        template: Some("t".into()),
        model_account: "a".into(),
        model_tuple: "m".into(),
        model_effort: None,
        turns: Some(1),
        started_at_unix: Some(NOW - 600),
        ended_at_unix: None,
        finished: !matches!(status, "pending" | "running" | "paused" | "sealing"),
        fault: None,
        waiting_on: None,
        dormancy: plakous_host_protocol::views::NodeDormancy::default(),
        interrupt_cause: None,
        continuation: None,
    }
}

fn finished(id: &str, parent: &str, attempt: &str, ago: u64) -> CanvasNode {
    let mut node = node(id, Some(parent), "completed");
    node.attempt_status = Some(attempt.into());
    node.ended_at_unix = Some(NOW - ago);
    if attempt == "failed" {
        "failed".clone_into(&mut node.status);
    }
    node
}

fn shown(projection: &AgentProjection) -> Vec<(&str, bool)> {
    projection
        .rows
        .iter()
        .map(|row| (row.id.as_str(), row.dim))
        .collect()
}

#[test]
fn live_scope_shows_active_recent_and_the_ancestors_that_explain_them() {
    let nodes = vec![
        node("root", None, "idle"),
        finished("planner", "root", "completed", 9 * 60),
        node("verifier", Some("planner"), "running"),
        finished("web", "root", "completed", 60),
        finished("tests", "root", "completed", 3 * 60),
        finished("old", "root", "completed", 6 * 60),
        node("waiting", Some("root"), "interrupted"),
    ];
    let projection = project(&nodes, &BTreeSet::new(), NOW, AgentScope::Live);
    assert_eq!(
        shown(&projection),
        vec![
            ("root", false),
            ("planner", true),
            ("verifier", false),
            ("tests", true),
            ("waiting", false),
            ("web", false),
        ],
        "a terminal ancestor is dimmed, 2-5 minutes is dimmed, past 5 is hidden; \
             siblings with one start stamp order by id"
    );
    assert_eq!(projection.hidden, 1);
    assert_eq!(
        projection.counts,
        AgentCounts {
            active: 2,
            completed: 4,
            attention: 0
        }
    );
    let all = project(&nodes, &BTreeSet::new(), NOW, AgentScope::All);
    assert_eq!(all.rows.len(), nodes.len());
    assert_eq!(all.hidden, 0);
    assert!(all.rows.iter().find(|row| row.id == "old").unwrap().dim);
}

#[test]
fn attention_outlives_the_ttl_until_its_attempt_is_acknowledged() {
    let mut recovery = node("stuck", Some("root"), "recovery_required");
    recovery.ended_at_unix = Some(NOW - 3_600);
    let nodes = vec![
        node("root", None, "idle"),
        finished("tests", "root", "failed", 3_600),
        finished("quiet", "root", "completed_without_submission", 3_600),
        recovery,
    ];
    let projection = project(&nodes, &BTreeSet::new(), NOW, AgentScope::Live);
    assert_eq!(
        projection
            .rows
            .iter()
            .map(|row| (row.id.as_str(), row.attention))
            .collect::<Vec<_>>(),
        vec![
            ("root", None),
            ("quiet", Some(Attention::NoSubmission)),
            ("stuck", Some(Attention::RecoveryRequired)),
            ("tests", Some(Attention::Failed)),
        ]
    );
    assert_eq!(projection.counts.attention, 3);
    let acks = BTreeSet::from([occurrence("tests", "failed", NOW - 3_600)]);
    let acked = project(&nodes, &acks, NOW, AgentScope::Live);
    assert_eq!(
        acked.position("tests"),
        None,
        "acknowledged and old: hidden"
    );
    assert_eq!(acked.counts.attention, 2);
}

fn occurrence(id: &str, attention: &str, ended_at: u64) -> AgentAcknowledgement {
    AgentAcknowledgement {
        agent_id: id.to_owned(),
        attempt_id: format!("{id}-a1"),
        attention: attention.to_owned(),
        ended_at: Some(ended_at),
    }
}

/// Acknowledging "needs recovery" does not cover a later refusal of that
/// same attempt: the end stamp stays and the label changes, so it counts
/// again.
#[test]
fn an_acknowledgement_covers_one_occurrence_not_the_whole_attempt() {
    let mut recovery = node("stuck", Some("root"), "recovery_required");
    recovery.attempt_status = Some("recovery_required".into());
    recovery.ended_at_unix = Some(NOW - 100);
    let acks = BTreeSet::from([AgentAcknowledgement {
        agent_id: "stuck".into(),
        attempt_id: "stuck-a1".into(),
        attention: Attention::RecoveryRequired.label().into(),
        ended_at: recovery.ended_at_unix,
    }]);
    let acked = project(
        &[node("root", None, "idle"), recovery.clone()],
        &acks,
        NOW,
        AgentScope::Live,
    );
    assert_eq!(
        acked.counts.attention, 0,
        "the recovery occurrence is covered"
    );
    recovery.status = "failed".into();
    recovery.attempt_status = Some("failed".into());
    let refused = project(
        &[node("root", None, "idle"), recovery],
        &acks,
        NOW,
        AgentScope::Live,
    );
    assert_eq!(
        refused
            .rows
            .iter()
            .find(|row| row.id == "stuck")
            .and_then(|row| row.attention),
        Some(Attention::Failed)
    );
    assert_eq!(
        refused.counts.attention, 1,
        "a refused recovery is a new occurrence and counts"
    );
    let mut again = node("stuck", Some("root"), "failed");
    again.attempt_status = Some("failed".into());
    again.ended_at_unix = Some(NOW - 5);
    let previous_failure = BTreeSet::from([occurrence("stuck", "failed", NOW - 100)]);
    let restamped = project(
        &[node("root", None, "idle"), again],
        &previous_failure,
        NOW,
        AgentScope::Live,
    );
    assert_eq!(
        restamped.counts.attention, 1,
        "a new end stamp is not covered"
    );
}

#[test]
fn a_followup_supersedes_attention_and_an_old_ack_does_not_cover_the_new_attempt() {
    let acks = BTreeSet::from([occurrence("tests", "failed", NOW - 3_600)]);
    let mut retask = node("tests", Some("root"), "running");
    retask.attempt_id = Some("tests-a2".into());
    retask.attempt_status = Some("running".into());
    let running = project(
        &[node("root", None, "idle"), retask.clone()],
        &BTreeSet::new(),
        NOW,
        AgentScope::Live,
    );
    assert_eq!(
        running.rows[1].attention, None,
        "no ack needed once superseded"
    );
    assert_eq!(running.counts.active, 1);
    retask.status = "failed".into();
    retask.attempt_status = Some("failed".into());
    retask.ended_at_unix = Some(NOW - 3_600);
    let failed_again = project(
        &[node("root", None, "idle"), retask],
        &acks,
        NOW,
        AgentScope::Live,
    );
    assert_eq!(failed_again.rows[1].attention, Some(Attention::Failed));
}

/// Spec 02 §5.7: glyph, elapsed, id, role, then one live fact right-aligned.
/// Model, turns, tokens and process count stay off the row. A narrow row
/// drops or truncates the role before the fact, and keeps glyph, elapsed,
/// id, fact and the mark.
#[test]
fn a_row_reads_left_to_right_and_drops_the_role_before_the_fact() {
    let facts = RowFacts {
        glyph: "\u{2715}",
        elapsed: "3m04s",
        short_id: "#107",
        fact: "failed",
        role: "staff-worker",
    };
    let wide = compose_row("  ├─ ", &facts, true, 110);
    assert!(
        wide.body.starts_with("\u{2715}  3m04s  #107  staff-worker"),
        "{wide:?}"
    );
    assert!(wide.body.ends_with("  failed"), "{wide:?}");
    assert!(!wide.body.contains("grok"), "{wide:?}");
    assert!(!wide.body.contains("tok"), "{wide:?}");
    assert!(!wide.body.contains("process"), "{wide:?}");
    assert_eq!(wide.mark, "  \u{26a0}");
    let narrow = compose_row("  ├─ ", &facts, true, 60);
    assert!(
        narrow
            .body
            .starts_with("\u{2715}  3m04s  #107  staff-worker"),
        "{narrow:?}"
    );
    assert!(narrow.body.ends_with("failed"), "{narrow:?}");
    let status_only = compose_row("  ├─ ", &facts, true, 36);
    assert!(
        status_only.body.starts_with("\u{2715}  3m04s  #107 "),
        "{status_only:?}"
    );
    assert!(status_only.body.ends_with(" failed"), "{status_only:?}");
    assert!(!status_only.body.contains("staff"), "{status_only:?}");
    let tiny = compose_row("  ├─ ", &facts, true, 20);
    assert!(
        tiny.body.contains("failed") || tiny.body.contains('…'),
        "{tiny:?}"
    );
    assert!(!tiny.body.contains("staff"), "{tiny:?}");
    for row in [&wide, &narrow, &status_only, &tiny] {
        assert!(!row.body.contains("tests"), "the description left the row");
    }
    for (row, width) in [(&wide, 110), (&narrow, 60), (&status_only, 36), (&tiny, 20)] {
        assert!(
            UnicodeWidthStr::width(format!("{}{}{}", row.rails, row.body, row.mark).as_str())
                <= width
        );
    }
}

/// The identity (glyph, elapsed, `#id`) does not move when the fact changes,
/// and the status column ends in the same cell on every row, marked or not.
/// The role may yield cells to a longer fact. At 40 columns a nested rail
/// still keeps the id.
#[test]
fn the_status_column_is_stable_across_status_changes_and_rows() {
    for width in [40, 60, 80, 120] {
        let facts = |fact| RowFacts {
            glyph: "\u{25cf}",
            elapsed: "1m02s",
            short_id: "#c3",
            fact,
            role: "staff-worker",
        };
        let identity = compose_row("  ├─ ", &facts(""), false, width);
        let identity = identity.body.split("  staff").next().unwrap_or("");
        assert!(identity.contains("#c3"), "{width}: {identity}");
        let mut ends = Vec::new();
        for (fact, attention) in [
            ("waiting", false),
            ("thinking", false),
            (
                "running cargo nextest run --workspace --all-features",
                false,
            ),
            ("waiting on p3, c2", false),
            ("failed", true),
            ("", false),
        ] {
            let row = compose_row("  ├─ ", &facts(fact), attention, width);
            let body_width = UnicodeWidthStr::width(row.body.as_str());
            assert!(
                row.body.starts_with(identity),
                "{width}: the identity moved for {fact:?}: {row:?}"
            );
            assert!(row.body.contains("#c3"), "{width}: the id was cut: {row:?}");
            ends.push(UnicodeWidthStr::width(row.rails.as_str()) + body_width);
            assert_eq!(UnicodeWidthStr::width(row.mark.as_str()), MARK_CELLS);
            let status = row.body.trim_end();
            let status = status.rsplit("  ").next().unwrap_or("");
            if !fact.is_empty() {
                let first = fact.split(' ').next().unwrap_or_default();
                assert!(
                    status.starts_with(first) || fact.contains(status.trim_end_matches('…')),
                    "{width}: the status {fact:?} shows: {row:?}"
                );
                let shown = fact.strip_prefix("waiting ").unwrap_or(fact);
                let floor = shown.len().min(STATUS_RESERVE);
                let room = body_width.saturating_sub(UnicodeWidthStr::width(identity) + 2);
                assert!(
                    UnicodeWidthStr::width(status) >= floor.min(room),
                    "{width}: {fact:?} got {status:?}"
                );
            }
            if fact.starts_with("waiting on") {
                assert!(
                    row.body.contains("p3"),
                    "{width}: the ids were cut: {row:?}"
                );
            }
        }
        assert!(
            ends.windows(2).all(|pair| pair[0] == pair[1]),
            "{width}: the status column moved: {ends:?}"
        );
    }
    let nested = compose_row(
        "  │ └─ ",
        &RowFacts {
            glyph: "\u{2713}",
            elapsed: "1m06s",
            short_id: "#105",
            fact: "done",
            role: "web",
        },
        false,
        36,
    );
    assert!(
        nested.body.contains("#105"),
        "a 40-column nested row keeps the id: {nested:?}"
    );
    assert!(!nested.body.contains("#…"), "{nested:?}");
    // The 40-column research row has one cell left for the role. That is not
    // enough for a truncated label, so the role is omitted rather than `…`.
    let tight = compose_row(
        "  ├─ ",
        &RowFacts {
            glyph: "⠋",
            elapsed: "8m12s",
            short_id: "#102",
            fact: "Searching",
            role: "research",
        },
        false,
        36,
    );
    assert!(
        !tight.body.contains('…'),
        "fewer than three cells left omits the role: {tight:?}"
    );
    assert!(tight.body.contains("#102"), "{tight:?}");
    assert!(tight.body.contains("Searching"), "{tight:?}");
}

#[test]
fn nested_children_keep_ancestor_rails() {
    let layout = TreeLayout::new(&[
        layout_node("root", None),
        layout_node("a", Some("root")),
        layout_node("a1", Some("a")),
        layout_node("b", Some("root")),
    ]);
    let prefixes: Vec<String> = layout
        .placements()
        .iter()
        .map(|placement| outline_prefix(&layout, placement))
        .collect();
    assert_eq!(prefixes, vec!["◆ ", "  ├─ ", "  │ └─ ", "  └─ "]);
}

#[test]
fn malformed_links_degrade_to_a_forest() {
    let layout = TreeLayout::new(&[
        layout_node("a", Some("b")),
        layout_node("b", Some("a")),
        layout_node("c", Some("missing")),
    ]);
    assert_eq!(layout.placements().len(), 3);
}

#[test]
fn dormant_rows_dim_at_once_and_parked_live_work_stays_in_live() {
    use plakous_host_protocol::views::NodeDormancy;
    let mut parked = node("parked", Some("root"), "running");
    parked.dormancy.parked = true;
    parked.waiting_on = Some(vec!["p3".into()]);
    let mut retired = finished("old", "root", "completed", 3_600);
    retired.dormancy.conversation_retired = true;
    let mut recent = finished("recent", "root", "completed", 30);
    recent.dormancy.workspace_reclaimed = true;
    let mut failed = finished("failed", "root", "failed", 3_600);
    failed.dormancy.closed = true;
    failed.dormancy.conversation_retired = true;
    let live = project(
        &[
            node("root", None, "idle"),
            parked.clone(),
            retired.clone(),
            recent.clone(),
            failed.clone(),
        ],
        &BTreeSet::new(),
        NOW,
        AgentScope::Live,
    );
    let row = |id: &str| live.rows.iter().find(|row| row.id == id);
    assert_eq!(row("parked").map(|row| row.dim), Some(true));
    assert!(
        row("old").is_none(),
        "an old retired row is not newly recent"
    );
    assert_eq!(row("recent").map(|row| row.dim), Some(true));
    assert_eq!(
        row("failed").map(|row| (row.dim, row.attention.is_some())),
        Some((true, true))
    );
    let all = project(
        &[node("root", None, "idle"), parked, retired, recent, failed],
        &BTreeSet::new(),
        NOW,
        AgentScope::All,
    );
    assert!(all.rows.iter().any(|row| row.id == "old" && row.dim));
    let _ = NodeDormancy::default();
}

#[test]
fn truncation_is_unicode_safe() {
    assert_eq!(truncate_display("日本語", 5), "日本…");
    assert_eq!(truncate_display("hello", 3), "he…");
}

/// Header `completed` is a successful attempt, not every settled node.
/// Closing is a resolution: it is neither completed nor attention.
#[test]
fn completed_counts_only_completed_or_adopted_and_closing_drops_attention() {
    let mut adopted = finished("adopted", "root", "adopted", 30);
    adopted.status = "completed".into();
    let mut closed = finished("closed", "root", "closed", 30);
    closed.status = "closed".into();
    let mut cancelled = finished("cancelled", "root", "cancelled", 30);
    cancelled.status = "failed".into();
    let mut operator_closed = finished("shut", "root", "failed", 3_600);
    operator_closed.status = "closed".into();
    let projection = project(
        &[
            node("root", None, "idle"),
            finished("done", "root", "completed", 30),
            adopted,
            closed,
            cancelled,
            finished("failed", "root", "failed", 30),
            operator_closed,
        ],
        &BTreeSet::new(),
        NOW,
        AgentScope::Live,
    );
    assert_eq!(
        projection.counts.completed, 2,
        "only completed and adopted attempts"
    );
    assert_eq!(
        projection.counts.attention, 1,
        "closed and cancelled are not attention, and closing a failed node resolves it"
    );
    assert!(
        projection
            .rows
            .iter()
            .find(|row| row.id == "shut")
            .is_none_or(|row| row.attention.is_none())
    );
}

/// The root and a node with no attempt cannot be acknowledged, so they
/// must not show an attention mark that `x` cannot clear.
#[test]
fn the_root_and_attemptless_nodes_never_carry_attention() {
    let mut root = node("root", None, "failed");
    root.finished = true;
    let mut bare = node("bare", Some("root"), "failed");
    bare.attempt_id = None;
    bare.attempt_status = None;
    bare.finished = true;
    bare.ended_at_unix = Some(NOW - 10);
    let projection = project(
        &[root, bare, finished("tests", "root", "failed", 10)],
        &BTreeSet::new(),
        NOW,
        AgentScope::Live,
    );
    assert_eq!(
        projection
            .rows
            .iter()
            .filter(|row| row.attention.is_some())
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        vec!["tests"]
    );
}
