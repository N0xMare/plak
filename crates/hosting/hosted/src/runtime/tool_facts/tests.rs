use super::*;

#[test]
fn a_wait_agent_starts_with_its_effective_ceiling() {
    let args = serde_json::json!({ "agent_ids": ["c39"], "timeout_ms": 3_600_000 });
    assert_eq!(
        wait_started("wait_agent", None, Some(&args)),
        Some(WaitFacts {
            timeout_ms: Some(3_600_000),
            ..WaitFacts::default()
        })
    );
    let defaulted = wait_started("wait_agent", None, Some(&serde_json::json!({})))
        .unwrap()
        .timeout_ms;
    assert_eq!(
        defaulted,
        Some(plakous_host_protocol::topology::DEFAULT_WAIT_TIMEOUT_MS)
    );
    assert_eq!(wait_started("read_file", None, Some(&args)), None);
    // Pi's `wait` over a child reads as `wait_agent` with its own default.
    let pi = serde_json::json!({ "ids": ["c39", "p1"] });
    assert_eq!(
        wait_started("wait_agent", None, Some(&pi))
            .unwrap()
            .timeout_ms,
        Some(plakous_host_protocol::topology::DEFAULT_WAIT_TIMEOUT_MS)
    );
    let poll = serde_json::json!({ "session_id": 7, "yield_time_ms": 30_000 });
    assert_eq!(
        wait_started("exec_command", Some("process_session_wait"), Some(&poll))
            .unwrap()
            .timeout_ms,
        Some(30_000)
    );
}

#[test]
fn a_settled_wait_names_the_agent_its_result_mail_delivered() {
    let started = wait_started("wait_agent", None, Some(&serde_json::json!({})));
    let result = serde_json::json!({
        "agents": [
            { "agent_id": "c38", "status": "completed", "attempt_id": "c38-a1" },
            { "agent_id": "c39", "status": "completed", "attempt_id": "c39-a2" },
        ],
        "mail": [{ "message_id": "m1", "purpose": "result", "attempt_id": "c39-a2" }],
        "timed_out": false,
        "woke_by": "settled",
    });
    let facts = wait_settled(
        "wait_agent",
        started,
        Some(&result),
        true,
        Some(720_000),
        None,
    )
    .unwrap();
    assert_eq!(facts.woke_by.as_deref(), Some("c39 finished"));
    assert_eq!(facts.settled_agent.as_deref(), Some("c39"));
    assert_eq!(facts.waited_ms, Some(720_000));
    assert_eq!(
        facts.timeout_ms,
        Some(plakous_host_protocol::topology::DEFAULT_WAIT_TIMEOUT_MS)
    );
}

#[test]
fn mail_timeout_and_failure_wakes_are_named() {
    let started = || wait_started("wait_agent", None, Some(&serde_json::json!({})));
    let woke = |result: Value, succeeded: bool| {
        wait_settled(
            "wait_agent",
            started(),
            Some(&result),
            succeeded,
            Some(5),
            None,
        )
        .unwrap()
    };
    let mail = woke(
        serde_json::json!({ "agents": [], "mail": [{ "from": "c7", "purpose": "question" }], "woke_by": "mail" }),
        true,
    );
    assert_eq!(mail.woke_by.as_deref(), Some("mail from c7"));
    assert_eq!(mail.settled_agent, None);
    let timeout = woke(
        serde_json::json!({ "agents": [], "mail": [], "timed_out": true, "woke_by": "timeout" }),
        true,
    );
    assert_eq!(timeout.woke_by.as_deref(), Some("timeout"));
    let refused = woke(
        serde_json::json!({ "code": "wait_caller_settled", "message": "abandoned" }),
        false,
    );
    assert_eq!(refused.woke_by, None);
    assert_eq!(refused.waited_ms, Some(5));
    let process = wait_settled(
        "exec_command",
        wait_started("exec_command", Some("process_session_wait"), None),
        Some(&serde_json::json!({ "session_id": 7, "exit_code": 0 })),
        true,
        Some(9),
        Some(&["7".to_owned()]),
    )
    .unwrap();
    assert_eq!(process.woke_by.as_deref(), Some("exited 0"));
}

#[test]
fn adopt_facts_earn_adopted_and_conflict_milestones() {
    let producer = |candidate: &str| {
        (candidate == "cand-c23-a1").then(|| ("c23".to_owned(), Some("worker".to_owned())))
    };
    let adopted = adopt_milestone(
        "cand-c23-a1",
        TopologyCallFacts::Adopted { files_changed: 16 },
        producer,
    )
    .unwrap();
    assert_eq!(adopted.event, MilestoneEvent::Adopted);
    assert_eq!(adopted.agent_id, "c23");
    assert_eq!(adopted.template.as_deref(), Some("worker"));
    assert_eq!(adopted.files, Some(16));
    let conflict = adopt_milestone(
        "cand-c23-a1",
        TopologyCallFacts::AdoptConflict {
            paths: vec!["src/lib.rs".to_owned(), "README.md".to_owned()],
            candidate: Some("cand-c23-a1-x1".to_owned()),
        },
        producer,
    )
    .unwrap();
    assert_eq!(conflict.event, MilestoneEvent::AdoptConflict);
    assert_eq!(conflict.paths, ["src/lib.rs", "README.md"]);
    assert_eq!(conflict.candidate.as_deref(), Some("cand-c23-a1-x1"));
    let unresolved = adopt_milestone(
        "cand-c7-a2",
        TopologyCallFacts::Adopted { files_changed: 1 },
        |_| None,
    )
    .unwrap();
    assert_eq!(
        unresolved.agent_id, "c7",
        "the candidate id names its sealer"
    );
    assert_eq!(unresolved.template, None);
}

/// Conflict paths come whole from the adopt's own facts: a path with
/// `, ` in its name stays one path, a long list keeps its first
/// [`MAX_CONFLICT_PATHS`] real paths, and no omission marker or spliced
/// fragment of a clipped refusal can become a path.
#[test]
fn conflict_paths_are_whole_paths_from_the_adopt_facts() {
    let mut paths: Vec<String> = (0..150)
        .map(|index| format!("crates/hosting/hosted/src/runtime/host_loop/module_{index:03}.rs"))
        .collect();
    paths.insert(0, "docs/notes, draft.md".to_owned());
    let conflict = adopt_milestone(
        "cand-c23-a1",
        TopologyCallFacts::AdoptConflict {
            paths: paths.clone(),
            candidate: None,
        },
        |_| None,
    )
    .unwrap();
    assert_eq!(conflict.paths, paths[..MAX_CONFLICT_PATHS]);
    assert_eq!(conflict.paths[0], "docs/notes, draft.md");
}
