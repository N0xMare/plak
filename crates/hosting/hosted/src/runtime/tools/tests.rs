#![allow(
    clippy::too_many_lines,
    reason = "Process-subject scenarios distinguish start, wait, read, input, and stop against a scoped registry that survives restore."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;

use crate::runtime::test_support::*;

/// Pi's `wait {ids}` and `stop {id}` rows name their process ids, and a
/// bare pi `wait` (a 300 s await by default) is labelled a wait, not a
/// snapshot read.
#[test]
fn pi_wait_and_stop_rows_name_their_process_ids() {
    assert_eq!(
        process_observation_label("process_session_poll", Some("wait"), &serde_json::json!({})),
        Some("process_session_wait")
    );
    assert_eq!(
        process_subject(
            "process_session_wait",
            &serde_json::json!({"ids": ["p1"], "timeout_ms": 5000}),
            None,
            &[],
            None,
        ),
        "1"
    );
    assert_eq!(
        process_subject(
            "process_session_terminate",
            &serde_json::json!({"id": "p2"}),
            None,
            &[],
            None,
        ),
        "2"
    );
}

#[test]
fn process_subject_routes_distinguish_start_wait_read_input_and_stop() {
    assert_eq!(
        process_observation_label(
            "process_session_poll",
            Some("BashOutput"),
            &serde_json::json!({"bash_id":"p7"})
        ),
        Some("process_session_wait")
    );
    assert_eq!(
        process_observation_label(
            "process_session_poll",
            Some("BashOutput"),
            &serde_json::json!({"session_id":7,"yield_time_ms":0})
        ),
        Some("process_session_read")
    );
    for (name, args, expected) in [
        ("Bash", serde_json::json!({"command":"cargo test"}), "Shell"),
        (
            "BashOutput",
            serde_json::json!({"bash_id":"p7"}),
            "process_session_wait",
        ),
        (
            "BashOutput",
            serde_json::json!({"bash_id":"p7", "block":false}),
            "process_session_read",
        ),
        (
            "TaskStop",
            serde_json::json!({"task_id":"p7"}),
            "process_session_terminate",
        ),
        (
            "KillShell",
            serde_json::json!({"shell_id":"7"}),
            "process_session_terminate",
        ),
        (
            "exec_command",
            serde_json::json!({"cmd":"cargo test"}),
            "Shell",
        ),
        (
            "exec_command",
            serde_json::json!({"session_id":7}),
            "process_session_wait",
        ),
        (
            "exec_command",
            serde_json::json!({"session_id":7,"yield_time_ms":0}),
            "process_session_read",
        ),
        (
            "write_stdin",
            serde_json::json!({"session_id":7,"chars":""}),
            "process_session_wait",
        ),
        (
            "write_stdin",
            serde_json::json!({"session_id":7,"chars":"yes\n"}),
            "process_session_write",
        ),
        (
            "write_stdin",
            serde_json::json!({"session_id":7,"chars":"\u{3}","tty":false}),
            "process_session_interrupt",
        ),
        (
            "run_terminal_command",
            serde_json::json!({"command":"cargo test"}),
            "Shell",
        ),
        (
            "get_command_or_subagent_output",
            serde_json::json!({"task_ids":["p7"],"timeout_ms":0}),
            "process_session_read",
        ),
        (
            "get_command_or_subagent_output",
            serde_json::json!({"task_ids":["p7"],"timeout_ms":1000}),
            "process_session_wait",
        ),
        (
            "kill_command_or_subagent",
            serde_json::json!({"task_id":"p7"}),
            "process_session_terminate",
        ),
        (
            "process_session_start",
            serde_json::json!({"command":"cargo test"}),
            "Shell",
        ),
        (
            "process_session_write",
            serde_json::json!({"session_id":7,"input":""}),
            "process_session_wait",
        ),
        (
            "process_session_write",
            serde_json::json!({"session_id":7,"input":"yes"}),
            "process_session_write",
        ),
        (
            "process_session_poll",
            serde_json::json!({"session_id":7,"yield_time_ms":0}),
            "process_session_read",
        ),
        (
            "process_session_poll",
            serde_json::json!({"session_id":7,"yield_time_ms":1000}),
            "process_session_wait",
        ),
        (
            "process_session_terminate",
            serde_json::json!({"session_id":7}),
            "process_session_terminate",
        ),
    ] {
        assert_eq!(process_label(name, &args), Some(expected), "{name}");
        assert!(
            !process_subject(expected, &args, None, &[], None).is_empty(),
            "{name}"
        );
    }
}

#[test]
fn process_subject_uses_the_scoped_registry_and_survives_restore() {
    use plakous_host_protocol::views::{ProcessStatus, ProcessView};
    let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
    let mut process = ProcessView {
        id: format!("{}:{}:p7", locator.session_id(), locator.branch_id()),
        command: "cargo test".into(),
        owner: "root".into(),
        started_at: 0,
        elapsed_ms: 0,
        status: ProcessStatus::Running,
        output: String::new(),
    };
    let mut other_session = process.clone();
    other_session.id = format!("{}:{}:p7", uuid::Uuid::from_u128(9), locator.branch_id());
    other_session.command = "child session command".into();
    let mut other_branch = process.clone();
    other_branch.id = format!("{}:{}:p7", locator.session_id(), uuid::Uuid::from_u128(9));
    other_branch.command = "child branch command".into();
    let args = serde_json::json!({"session_id":7});
    assert_eq!(
        process_subject(
            "process_session_wait",
            &args,
            Some(locator),
            &[other_session.clone(), other_branch.clone()],
            None,
        ),
        "7"
    );
    for (result, expected) in [
        (
            serde_json::json!({"status":"running"}),
            "7 · cargo test · still running",
        ),
        (
            serde_json::json!({"exit_code":0}),
            "7 · cargo test · exited 0",
        ),
        (
            serde_json::json!({"exit_code":7}),
            "7 · cargo test · exited 7",
        ),
    ] {
        process.status = ProcessStatus::OutcomeUnknown;
        let subject = process_subject(
            "process_session_wait",
            &args,
            Some(locator),
            &[other_session.clone(), other_branch.clone(), process.clone()],
            Some(&result),
        );
        assert_eq!(subject, expected);
        let subject = process_action_subject("process_session_wait", &subject, true);
        let encoded = crate::sessions::ToolItem {
            call_id: "wait-7".into(),
            name: "process_session_poll".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("process_session_poll"),
            state: "done".into(),
            subject: Some(subject.clone()),
            detail: None,
            started_at_ms: None,
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        };
        assert_eq!(
            serde_json::from_value::<crate::sessions::ToolItem>(
                serde_json::to_value(&encoded).unwrap()
            )
            .unwrap()
            .subject
            .as_deref(),
            Some(subject.as_str())
        );
    }
    assert_eq!(
        process_subject(
            "process_session_wait",
            &serde_json::json!({"bash_id":"p8"}),
            Some(locator),
            std::slice::from_ref(&process),
            None
        ),
        "8"
    );
    assert_eq!(
        process_subject(
            "process_session_wait",
            &args,
            None,
            std::slice::from_ref(&process),
            None
        ),
        "7"
    );
    process.command = format!("password={} {}", "x".repeat(250), "界".repeat(200));
    let subject = process_subject(
        "process_session_wait",
        &args,
        Some(locator),
        &[process],
        Some(&serde_json::json!({"exit_code":7})),
    );
    assert!(subject.len() <= 240);
    assert!(subject.contains("password=[redacted]"));
    assert!(!subject.contains(&"x".repeat(10)));
    assert!(subject.ends_with(" · exited 7"));
    let subject = process_action_subject("process_session_wait", &subject, true);
    assert!(subject.len() <= 240);
    assert!(subject.ends_with(" · exited 7"));
    let input = process_subject(
        "process_session_write",
        &serde_json::json!({"session_id":7,"chars":format!("password={}\n{}", "x".repeat(250), "界".repeat(200))}),
        None,
        &[],
        None,
    );
    assert!(input.len() <= 240);
    assert_eq!(input, "7 · 860 bytes");
    let input = process_action_subject("process_session_write", &input, false);
    assert!(input.len() <= 240);
    assert_eq!(input, "Sent input to process 7 · 860 bytes");
}

#[test]
fn process_wait_outcomes_are_observation_local() {
    for (result, expected) in [
        (
            serde_json::json!({"status":"running","exit_code":null}),
            Some("still running"),
        ),
        (
            serde_json::json!({"status":"terminated","exit_code":130}),
            Some("stopped"),
        ),
        (serde_json::json!({"exit_code":7}), Some("exited 7")),
        (serde_json::json!({"session_id":7}), Some("still running")),
        (
            serde_json::json!({"tasks":[{"task_id":"p8","exit_code":0},{"task_id":"p7","status":"running"}]}),
            Some("still running"),
        ),
        (
            serde_json::json!({"tasks":[{"task_id":"p8","exit_code":0}]}),
            None,
        ),
        (serde_json::json!({"status":"not_found"}), None),
        (serde_json::Value::Null, None),
    ] {
        assert_eq!(process_wait_outcome(&result, "7").as_deref(), expected);
    }
}

#[test]
fn process_ids_never_display_model_text() {
    for (raw, expected) in [
        (" p7 ", "7"),
        ("p007", "7"),
        ("0", "(unknown)"),
        ("18446744073709551616", "(unknown)"),
        ("pp7", "(unknown)"),
        ("7 command", "(unknown)"),
        ("p", "(unknown)"),
    ] {
        let subject = process_subject(
            "process_session_read",
            &serde_json::json!({"session_id":raw}),
            None,
            &[],
            None,
        );
        assert_eq!(subject, expected);
    }
}

/// Text blocks of one answer never run together mid-sentence.
#[test]
fn answer_text_blocks_are_separated_unless_they_end_a_line() {
    use plakous::model::{
        Actor, ConversationItem, Digest, DigestDomain, EntryId, FinishDetails, FinishReason,
        ModelResponse, Provenance, TextBlock,
    };
    use std::str::FromStr as _;
    let block = |text: &str| ContentBlock::Text(TextBlock::new(text, None).unwrap());
    let item = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000101").unwrap(),
        Actor::Assistant,
        vec![
            block("I checked, and it is actually there."),
            block("`calc/ops.py` has four functions.\n"),
            block("Done."),
        ],
        Provenance::ModelOutput,
    )
    .unwrap();
    let response = ModelResponse::new(
        vec![item],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan"]).unwrap(),
    );
    assert_eq!(
        joined_assistant_text(&response),
        "I checked, and it is actually there.\n\n`calc/ops.py` has four functions.\nDone."
    );
}

#[test]
fn subject_preview_redacts_before_clipping() {
    let command = format!("{}token=abcdefgh", "x ".repeat(74));
    let args = serde_json::json!({"cmd": command});
    let subject = tool_subject_from_args("exec_command", &args).unwrap();
    assert_eq!(subject, format!("{}token=[redac", "x ".repeat(74)));
    assert_eq!(args["cmd"], command);
}

/// The `ResponseCompleted` stash was empty at every
/// `ToolStarted`, so the subject has to come from the started event's own
/// observation, which carried none for `apply_patch` and `wait`
/// (rows rendered bare with a JSON dump).
#[test]
fn started_observation_yields_edit_wait_and_shell_subjects_without_a_stash() {
    let patch = "*** Begin Patch\n*** Update File: notes.txt\n@@\n-BETA\n+beta\n*** End Patch";
    let edit = started_observation(&serde_json::json!({ "patch": patch }));
    assert_eq!(
        tool_subject_from_observation("apply_patch", Some("apply_patch"), &edit).as_deref(),
        Some("notes.txt +1/-1")
    );
    let wait = started_observation(&serde_json::json!({
        "cell_id": "1",
        "max_tokens": 4096,
        "terminate": false,
        "yield_time_ms": 30000,
    }));
    assert_eq!(
        tool_subject_from_observation("wait", Some("wait"), &wait).as_deref(),
        Some("cell 1 · up to 30s")
    );
    assert_eq!(
        tool_subject_from_observation("wait_py", Some("wait_py"), &wait).as_deref(),
        Some("cell 1 · up to 30s")
    );
    let terminate = started_observation(&serde_json::json!({
        "cell_id": "1",
        "terminate": true,
    }));
    assert_eq!(
        tool_subject_from_observation("wait", None, &terminate).as_deref(),
        Some("cell 1 · terminate")
    );
    // The exposed name shapes the arguments: `exec_command { cmd }` is
    // the canonical `process_session_start`.
    let shell = started_observation(&serde_json::json!({ "cmd": "ls -1", "shell": "/bin/sh" }));
    assert_eq!(
        tool_subject_from_observation("process_session_start", Some("exec_command"), &shell)
            .as_deref(),
        Some("ls -1")
    );
    // A preview truncated past the observation bound is not a subject.
    let truncated: plakous::agent::ToolCallObservation = serde_json::from_value(
        serde_json::json!({ "subject": null, "arguments_preview": "{\n  \"patch\": \"*** Begin" }),
    )
    .unwrap();
    assert!(
        tool_subject_from_observation("apply_patch", Some("apply_patch"), &truncated).is_none()
    );
}

/// Pi's `wait` is spelled like the Code Mode cell-wait marker but its
/// identity is the process poll: its subject names the processes it
/// awaits, never a cell.
#[test]
fn a_pi_wait_subject_names_processes_not_a_cell() {
    let wait = started_observation(&serde_json::json!({ "ids": ["p3"], "cell_id": "9" }));
    let subject =
        tool_subject_from_observation("process_session_poll", Some("wait"), &wait).unwrap();
    assert!(subject.contains('3'), "{subject}");
    assert!(!subject.contains("cell"), "{subject}");
    let bare = started_observation(&serde_json::json!({}));
    assert_eq!(
        process_observation_label("process_session_poll", Some("wait"), &serde_json::json!({})),
        Some("process_session_wait")
    );
    assert!(
        !tool_subject_from_observation("process_session_poll", Some("wait"), &bare)
            .unwrap_or_default()
            .contains("cell")
    );
}

#[test]
fn web_and_mcp_rows_carry_host_subjects() {
    let subject = |name: &str, args: serde_json::Value| tool_subject_from_args(name, &args);
    assert_eq!(
        subject(
            "image_gen",
            serde_json::json!({"prompt": "a fox", "path": "art/fox.png", "size": "1024x1024"})
        )
        .as_deref(),
        Some("art/fox.png (1024x1024)")
    );
    assert_eq!(
        subject(
            "image_gen",
            serde_json::json!({"prompt": "a fox", "path": "fox.png"})
        )
        .as_deref(),
        Some("fox.png")
    );
    for name in ["web_search", "WebSearch", "x_search"] {
        assert_eq!(
            subject(name, serde_json::json!({ "query": "rust 1.99" })).as_deref(),
            Some("\"rust 1.99\""),
            "{name}"
        );
    }
    for name in ["web_fetch", "WebFetch", "fetch_url", "fetch", "open_url"] {
        assert_eq!(
            subject(
                name,
                serde_json::json!({
                    "url": "https://user:pw@docs.example.org:8443/a/b?token=x#frag",
                    "prompt": "summarize"
                })
            )
            .as_deref(),
            Some("docs.example.org:8443"),
            "{name}"
        );
    }
    assert_eq!(
        subject(
            "mcp__docs__search",
            serde_json::json!({ "query": "install", "limit": 5, "filters": { "a": 1 } })
        )
        .as_deref(),
        Some("filters=… limit=5 query=\"install\"")
    );
    let long = subject(
        "mcp__docs__search",
        serde_json::json!({ "a": "x".repeat(60), "b": "y".repeat(60), "c": "z".repeat(60) }),
    )
    .unwrap();
    assert!(
        long.len() <= MCP_ARGUMENTS_SUBJECT_BYTES + '…'.len_utf8(),
        "{long}"
    );
    assert!(long.ends_with('…'), "{long}");
    assert_eq!(subject("mcp__docs__ping", serde_json::json!({})), None);
}

/// The runtime's generic subject (a raw query, a whole URL) never wins
/// over the host's own for web and MCP rows.
#[test]
fn web_and_mcp_host_subjects_win_over_the_core_subject() {
    let observation: plakous::agent::ToolCallObservation =
        serde_json::from_value(serde_json::json!({
            "subject": "https://docs.example.org/a?token=x",
            "arguments_preview": "{\"url\":\"https://docs.example.org/a?token=x\",\"prompt\":\"p\"}"
        }))
        .unwrap();
    assert_eq!(
        started_tool_subject("web_fetch", Some("WebFetch"), Some(&observation), None).as_deref(),
        Some("docs.example.org")
    );
    let observation: plakous::agent::ToolCallObservation = serde_json::from_value(
        serde_json::json!({ "subject": "install", "arguments_preview": "{\"query\":\"install\"}" }),
    )
    .unwrap();
    assert_eq!(
        started_tool_subject("mcp__docs__search", None, Some(&observation), None).as_deref(),
        Some("query=\"install\"")
    );
}

#[test]
fn host_diffstat_wins_over_the_bare_core_subject() {
    let observation: plakous::agent::ToolCallObservation = serde_json::from_value(serde_json::json!({"subject":"file.rs", "arguments_preview": "{\"file_path\":\"file.rs\",\"old_string\":\"old\",\"new_string\":\"new\\nline\"}"})).unwrap();
    assert_eq!(
        started_tool_subject("Edit", Some("Edit"), Some(&observation), None).as_deref(),
        Some("file.rs +2/-1")
    );
}

#[test]
fn adoption_names_the_candidate() {
    assert_eq!(
        tool_subject_from_args(
            "adopt_candidate",
            &serde_json::json!({"candidate_id": "candidate-7"})
        )
        .as_deref(),
        Some("candidate-7")
    );
}

#[test]
fn projected_process_tools_have_exact_product_subjects() {
    assert_eq!(
        tool_subject_from_args("exec_command", &serde_json::json!({"cmd":"cargo test"})),
        // §5.2 renders `Run {command}`; the verb is the row's, not the subject's.
        Some("cargo test".to_owned())
    );
    assert_eq!(
        tool_subject_from_args(
            "run_terminal_command",
            &serde_json::json!({"command":"cargo check","description":"check"}),
        ),
        Some("cargo check".to_owned())
    );
    assert_eq!(
        tool_subject_from_args(
            "run_terminal_cmd",
            &serde_json::json!({"command":"cargo check","description":"check"}),
        ),
        Some("cargo check".to_owned())
    );
    assert_eq!(
        tool_subject_from_args(
            "write_stdin",
            &serde_json::json!({"session_id":7,"chars":"yes\n"}),
        ),
        Some("7 · 4 bytes".to_owned())
    );
}

#[test]
fn oauth_costume_tool_subjects_use_wire_field_names() {
    assert_eq!(
        tool_subject_from_args("Bash", &serde_json::json!({"command":"printf hi"})),
        Some("printf hi".to_owned())
    );
    assert_eq!(
        tool_subject_from_args(
            "read_file",
            &serde_json::json!({"target_file":"src/lib.rs"})
        ),
        Some("src/lib.rs".to_owned())
    );
    assert_eq!(
        tool_subject_from_args("Read", &serde_json::json!({"file_path":"a.txt","limit":10})),
        Some("a.txt".to_owned())
    );
    assert_eq!(
        tool_subject_from_args(
            "write",
            &serde_json::json!({"file_path":"a.txt","content":"one\ntwo"})
        ),
        Some("a.txt +2/-0".to_owned())
    );
    assert_eq!(
        tool_subject_from_args("list_dir", &serde_json::json!({"target_directory":"src"})),
        Some("src".to_owned())
    );
    assert_eq!(
        tool_subject_from_args("merge_plan", &serde_json::json!({"todos":[]})),
        Some("0 items".to_owned())
    );
    assert_eq!(
        tool_subject_from_args(
            "todo_write",
            &serde_json::json!({
                "todos": [
                    {"id": "a", "content": "wrap", "status": "completed"},
                    {"id": "b", "content": "rail", "status": "in_progress"}
                ]
            })
        ),
        Some("2 items · 1 in progress".to_owned())
    );
    assert_eq!(
        tool_subject_from_args(
            "update_plan",
            &serde_json::json!({
                "plan": [{"step": "only", "status": "pending"}]
            })
        ),
        Some("1 item".to_owned())
    );
    assert_eq!(
        tool_subject_from_args("Grep", &serde_json::json!({"pattern":"TODO","path":"src"})),
        Some("\"TODO\" in src".to_owned())
    );
    assert_eq!(
        tool_subject_from_args("Glob", &serde_json::json!({"pattern":"**/*.rs"})),
        Some("\"**/*.rs\"".to_owned())
    );
}

/// `wait_agent` rendered as
/// `▸ ✓ Agent · 6.3s` and `list_agents` as a raw humanized tool name,
/// because neither had a subject arm and both were forced onto the
/// `Agent` verb.
#[test]
fn the_read_and_mail_topology_tools_carry_section_5_2_subjects() {
    let subject = |name: &str, args: serde_json::Value| tool_subject_from_args(name, &args);
    assert_eq!(
        subject(
            "wait_agent",
            serde_json::json!({ "agent_ids": ["c1", "c2"], "timeout_ms": 30_000 })
        )
        .as_deref(),
        Some("c1,c2 · up to 30s")
    );
    assert_eq!(
        subject("wait_agent", serde_json::json!({ "agent_ids": ["c1"] })).as_deref(),
        Some("c1")
    );
    assert_eq!(
        subject("wait_agent", serde_json::json!({ "agent_ids": [] })),
        None
    );
    assert_eq!(
        subject("list_agents", serde_json::json!({})).as_deref(),
        Some("agents")
    );
    assert_eq!(
        subject(
            "send_agent_message",
            serde_json::json!({ "agent_id": "c1", "message": "keep going" })
        )
        .as_deref(),
        Some("c1")
    );
}

#[test]
fn tool_subjects_match_the_normative_header_formats() {
    let subject = |name: &str, args: serde_json::Value| tool_subject_from_args(name, &args);
    assert_eq!(
        subject("exec_command", serde_json::json!({ "cmd": "cargo test" })).as_deref(),
        Some("cargo test")
    );
    assert_eq!(
        subject(
            "read_file",
            serde_json::json!({ "path": "src/lib.rs", "offset": 1, "limit": 120 })
        )
        .as_deref(),
        Some("src/lib.rs (1-120)")
    );
    assert_eq!(
        subject("read_file", serde_json::json!({ "path": "src/lib.rs" })).as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(
        subject(
            "search_repository",
            serde_json::json!({ "query": "ToolLine", "path": "crates" })
        )
        .as_deref(),
        Some("\"ToolLine\" in crates")
    );
    assert_eq!(
        subject("list_dir", serde_json::json!({ "path": "crates/plak" })).as_deref(),
        Some("crates/plak")
    );
    assert_eq!(
        subject("web_fetch", serde_json::json!({ "url": "https://a.test" })).as_deref(),
        Some("a.test")
    );
    assert_eq!(
        subject(
            "apply_patch",
            serde_json::json!({
                "patch": "*** Update File: src/ui.rs\n+one\n+two\n-old\n"
            })
        )
        .as_deref(),
        Some("src/ui.rs +2/-1")
    );
}

#[test]
fn completed_headers_take_their_counts_from_the_host_not_the_renderer() {
    // §5.2 admits no bare line count for Read; a complete but empty result
    // is the one thing the host can state about it.
    assert_eq!(tool_result_summary("read_file", Some("a\nb\nc")), None);
    assert_eq!(
        tool_result_summary("read_file", Some("")).as_deref(),
        Some("(empty)")
    );
    assert_eq!(
        tool_result_summary("search_repository", Some("a.rs:1:hit\nb.rs:2:hit")).as_deref(),
        Some("(2 matches in 2 files)")
    );
    assert_eq!(
        tool_result_summary("search_repository", Some("")).as_deref(),
        Some("(no matches)")
    );
    assert_eq!(
        tool_result_summary("list_dir", Some("a\nb")).as_deref(),
        Some("(2 entries)")
    );
    // A verb whose count the host cannot state truthfully adds nothing.
    assert_eq!(tool_result_summary("exec_command", Some("out")), None);
}

/// The observation the host counts is a 2 KiB bounded preview, so a
/// count over a truncated one is approximate and must say so (§5.1 "no
/// invented count"). A 5,000-line read never renders `(64 lines)`.
#[test]
fn counts_over_a_bounded_preview_are_marked_approximate_or_omitted() {
    let bounded = |body: &str| format!("{body}\n{TOOL_PREVIEW_TRUNCATION_MARKER}");
    let hits = (0..64)
        .map(|line| format!("a.rs:{line}:hit"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(!preview_is_complete(&bounded("a\nb")));
    assert!(preview_is_complete("a\nb"));
    assert!(!preview_is_complete(&"x".repeat(2 * 1024)));

    // Read states no count at all rather than the preview's own size.
    assert_eq!(
        tool_result_summary("read_file", Some(&bounded("a\nb"))),
        None
    );
    assert_eq!(
        tool_result_summary("search_repository", Some(&bounded(&hits))).as_deref(),
        Some("(~65 matches in ~1 files)")
    );
    assert_eq!(
        tool_result_summary("list_dir", Some(&bounded("a\nb"))).as_deref(),
        Some("(~3 entries)")
    );
    // A single observed hit in a bounded window is not "1 match".
    assert_eq!(
        tool_result_summary("search_repository", Some(&bounded("a.rs:1:hit"))).as_deref(),
        Some("(~2 matches in ~1 files)")
    );
}

/// `limit` counts lines, so `(1-120)` is offset 1 with limit 120.
#[test]
fn a_bounded_read_names_the_last_line_it_asked_for() {
    assert_eq!(
        tool_subject_from_args(
            "read_file",
            &serde_json::json!({ "path": "src/lib.rs", "offset": 1, "limit": 120 })
        )
        .as_deref(),
        Some("src/lib.rs (1-120)")
    );
    assert_eq!(
        tool_subject_from_args(
            "read_file",
            &serde_json::json!({ "path": "src/lib.rs", "offset": 40, "limit": 0 })
        )
        .as_deref(),
        Some("src/lib.rs (40-)")
    );
}

#[test]
fn tool_subject_stash_correlates_parallel_same_name_lifecycles_exactly() {
    let mut stash = ToolSubjectStash::new();
    stash.push(
        "exec_command".into(),
        Some("call-a".into()),
        pending_tool("same", "arguments-a"),
    );
    stash.push(
        "exec_command".into(),
        Some("call-b".into()),
        pending_tool("same", "arguments-b"),
    );

    assert!(
        stash.take("exec_command", None).is_none(),
        "missing identity must not guess between parallel same-name calls"
    );
    assert_eq!(
        stash
            .take("exec_command", Some("call-b"))
            .map(|info| info.detail),
        Some("arguments-b".into()),
        "reverse start/finish must use the exact call identity"
    );
    assert_eq!(
        stash.take("exec_command", None).map(|info| info.detail),
        Some("arguments-a".into()),
        "a missing identity is safe only after one exact candidate remains"
    );
    assert!(stash.take("exec_command", Some("call-a")).is_none());

    stash.push(
        "model_alias".into(),
        Some("rejected".into()),
        pending_tool("rejected", "rejected-arguments"),
    );
    assert!(
        stash.take("canonical_name", Some("rejected")).is_none(),
        "an alias mismatch fails closed"
    );
    assert!(stash.take("model_alias", None).is_none());

    stash.push(
        "exec_command".into(),
        None,
        pending_tool("unidentified", "must-not-survive"),
    );
    assert!(stash.take("exec_command", None).is_none());

    drop(stash);
    let mut restarted = ToolSubjectStash::new();
    assert!(restarted.take("exec_command", None).is_none());
}

/// A process tool row names its process's command as the runtime displays
/// it, newlines escaped: a heredoc's token on its own line is redacted
/// there too.
#[test]
fn a_process_rows_command_has_its_heredoc_secret_redacted() {
    use plakous_host_protocol::views::{ProcessStatus, ProcessView};
    let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
    let process = ProcessView {
        id: format!("{}:{}:p7", locator.session_id(), locator.branch_id()),
        command: "gh auth login --with-token <<EOF\\nghp_abcdefghijklmnopqrstuvwxyz0123\\nEOF"
            .into(),
        owner: "root".into(),
        started_at: 0,
        elapsed_ms: 0,
        status: ProcessStatus::Running,
        output: String::new(),
    };
    let subject = process_subject(
        "process_session_wait",
        &serde_json::json!({"session_id":7}),
        Some(locator),
        &[process],
        Some(&serde_json::json!({"status":"running"})),
    );
    assert_eq!(
        subject,
        "7 · gh auth login --with-token <<EOF\\n[redacted]\\nEOF · still running"
    );
}
