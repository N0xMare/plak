use super::*;

#[test]
fn shell_results_render_as_command_output_and_status() {
    let detail = tool_detail_text(
        Some(r#"{"command":"cargo test"}"#),
        Some(r#"{"stdout":"one\ntwo\n","stderr":"warning: slow","exit_code":2}"#),
    )
    .expect("shell projection");
    assert_eq!(detail, "$ cargo test\none\ntwo\nwarning: slow\nexit 2");
}

#[test]
fn a_process_that_ran_and_failed_is_named_by_how_it_ended() {
    assert_eq!(
        process_failure("{\n  \"exit_code\": 101,\n  \"output\": \"error\"\n}").as_deref(),
        Some("exit 101")
    );
    assert_eq!(process_failure(r#"{"exit_code":0,"output":"ok"}"#), None);
    assert_eq!(
        process_failure(r#"{"status":"timed_out","output":"partial"}"#).as_deref(),
        Some("timed out")
    );
    assert_eq!(
        process_failure(r#"{"result":"compiling\n[exit 2]"}"#).as_deref(),
        Some("exit 2")
    );
    assert_eq!(
        process_failure("partial output\n[timed out after 300s]").as_deref(),
        Some("timed out")
    );
    assert_eq!(
        process_failure("[killed: output limit]").as_deref(),
        Some("output limit")
    );
    // A plan snapshot's `status` is not a process status.
    assert_eq!(process_failure(r#"{"status":"failed","items":[]}"#), None);
    assert_eq!(process_failure("plain text"), None);
}

#[test]
fn successful_shell_results_omit_the_exit_line() {
    let detail = tool_detail_text(
        None,
        Some(r#"{"command":"pwd","stdout":"/tmp","exit_code":0}"#),
    )
    .expect("shell projection");
    assert_eq!(detail, "$ pwd\n/tmp");
}

#[test]
fn file_reads_render_as_content_lines() {
    let detail = tool_detail_text(
        Some(r#"{"path":"src/main.rs"}"#),
        Some(r#"{"path":"src/main.rs","content":"fn main() {}\n"}"#),
    )
    .expect("file projection");
    assert!(detail.ends_with("fn main() {}"), "unexpected: {detail}");
    assert!(!detail.contains("\\n"), "literal escape survived: {detail}");
}

#[test]
fn generic_json_is_pretty_printed_with_real_newlines() {
    let detail = tool_detail_text(None, Some(r#"{"summary":"first\nsecond","count":2}"#))
        .expect("generic projection");
    assert!(!detail.contains("\\n"), "literal escape survived: {detail}");
    assert!(detail.contains("first\n"), "unexpected: {detail}");
    assert!(detail.contains("second"), "unexpected: {detail}");
    assert!(detail.contains("\"count\": 2"), "unexpected: {detail}");
}

/// A multi-line string keeps its own indentation exactly: each line gets
/// the indentation of the JSON line it started on and nothing more.
#[test]
fn multi_line_arguments_are_not_cumulatively_reindented() {
    let detail = tool_detail_text(
        Some(
            r#"{"file_path":"calc.py","new_string":"def div(a, b):\n    # true division\n    return a / b\n"}"#,
        ),
        None,
    )
    .expect("generic projection");
    assert!(
        detail.contains(
            "\"new_string\": \"def div(a, b):\n      # true division\n      return a / b\n  \""
        ),
        "unexpected: {detail}"
    );
}

#[test]
fn plain_text_previews_and_empty_previews_pass_through() {
    assert_eq!(
        tool_detail_text(None, Some("already human text\n")),
        Some("already human text".to_owned())
    );
    assert_eq!(tool_detail_text(Some("   "), Some("")), None);
}

#[test]
fn json_string_results_lose_their_quotes() {
    assert_eq!(
        tool_detail_text(None, Some(r#""line one\nline two""#)),
        Some("line one\nline two".to_owned())
    );
}

/// The `apply_patch` and `wait` previews rendered as `{"patch": …}` and
/// `{"terminate": false, "yield_time_ms": 30000}` dumps (§5.7).
#[test]
fn edit_and_wait_previews_render_as_text_not_json() {
    let patch = "*** Begin Patch\n*** Update File: notes.txt\n@@\n-BETA\n+beta\n*** End Patch";
    let arguments = serde_json::json!({ "patch": patch }).to_string();
    let result = serde_json::json!({ "result": "changed_files=1" }).to_string();
    assert_eq!(
        tool_detail_text(Some(&arguments), Some(&result)),
        Some(format!("{patch}\n\nchanged_files=1"))
    );
    let wait = serde_json::json!({
        "cell_id": "1",
        "max_tokens": "[redacted]",
        "terminate": false,
        "yield_time_ms": 30000,
    })
    .to_string();
    assert_eq!(
        tool_detail_text(Some(&wait), None),
        Some("wait cell 1 · yield up to 30000 ms".to_owned())
    );
    let terminate = serde_json::json!({ "cell_id": 1, "terminate": true }).to_string();
    assert_eq!(
        tool_detail_text(Some(&terminate), None),
        Some("wait cell 1 · terminate".to_owned())
    );
    // A two-field object is not a "single string" and still pretty-prints.
    let two = serde_json::json!({ "a": "x", "b": "y" }).to_string();
    assert!(
        tool_detail_text(Some(&two), None)
            .unwrap()
            .contains("\"a\": \"x\"")
    );
}

#[test]
fn plan_payloads_render_as_bullets_not_json() {
    let arguments = serde_json::json!({
        "merge": true,
        "todos": [
            {"id": "a", "content": "wrap thoughts", "status": "completed"},
            {"id": "b", "content": "rail groups", "status": "in_progress"},
            {"id": "c", "content": "fence json", "status": "pending"}
        ]
    })
    .to_string();
    let detail = tool_detail_text(Some(&arguments), None).expect("plan projection");
    assert_eq!(
        detail,
        "- [completed] wrap thoughts\n- [in_progress] rail groups\n- [pending] fence json"
    );

    let snapshot = "revision=3\n- [completed] wrap thoughts\n- [in_progress] rail groups\n- [pending] fence json\n";
    let combined = tool_detail_text(Some(&arguments), Some(snapshot)).expect("merged plan");
    assert_eq!(combined, detail, "the snapshot wins over the write payload");
    assert!(!combined.contains("\"todos\""), "{combined}");
    assert!(!combined.contains("revision="), "{combined}");
}
