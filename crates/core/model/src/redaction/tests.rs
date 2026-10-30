use super::*;

#[test]
fn preview_redaction_mebibyte_smoke() {
    let input = "ordinary readable text ".repeat(48_000);
    assert!(input.len() >= 1024 * 1024);
    let started = std::time::Instant::now();
    assert_eq!(text(&input), input);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn escaped_quote_assignment_is_redacted_in_raw_text() {
    let raw = r#"{"cmd":"export API_TOKEN=\"hunter2hunter2\""#;
    let redacted = text(raw);
    assert!(!redacted.contains("hunter2"), "{redacted}");
    assert!(redacted.contains("[redacted]"), "{redacted}");
    assert_eq!(text(&redacted), redacted);
    let parsed = text(r#"export API_TOKEN="hunter2hunter2""#);
    assert!(!parsed.contains("hunter2"), "{parsed}");
}

#[test]
fn assignment_rule_stays_fast_on_a_long_backslash_run() {
    let input = format!("password={}", "\\".repeat(200_000));
    // CPU time, not wall time. The regex crate's linear scan of this run is
    // milliseconds of CPU; a backtracking scan is not. Wall time under gate
    // load includes time the scheduler stole and is not the contract.
    let started = process_cpu_time();
    let redacted = text(&input);
    let spent = process_cpu_time().saturating_sub(started);
    assert!(
        spent < std::time::Duration::from_secs(1),
        "backslash run used {spent:?} of CPU"
    );
    assert_eq!(redacted, "password=[redacted]");
    assert_eq!(text(&redacted), redacted);
}

fn process_cpu_time() -> std::time::Duration {
    use std::time::Duration;

    use nix::sys::resource::{UsageWho, getrusage};
    use nix::sys::time::TimeValLike as _;

    let usage = getrusage(UsageWho::RUSAGE_SELF).expect("process CPU time");
    let micros = usage
        .user_time()
        .num_microseconds()
        .saturating_add(usage.system_time().num_microseconds());
    Duration::from_micros(u64::try_from(micros).unwrap_or(0))
}

#[test]
fn credential_key_redacts_string_and_number_leaves_at_any_depth() {
    let mut value = serde_json::json!({
        "password": ["hunter2hunter2", 7],
        "api_key": {"v": "hunter3hunter3", "n": 1, "ok": false, "z": null},
        "cmd": "export API_TOKEN=\"hunter2hunter2\"",
        "token": ["readable"],
        "secret": false,
        "input_tokens": 123
    });
    json(&mut value);
    let rendered = value.to_string();
    assert!(
        !rendered.contains("hunter2") && !rendered.contains("hunter3"),
        "{rendered}"
    );
    assert_eq!(
        value["password"],
        serde_json::json!(["[redacted]", "[redacted]"])
    );
    assert_eq!(value["api_key"]["v"], "[redacted]");
    assert_eq!(value["api_key"]["n"], "[redacted]");
    assert_eq!(value["api_key"]["ok"], false);
    assert_eq!(value["api_key"]["z"], Value::Null);
    assert_eq!(value["token"], serde_json::json!(["[redacted]"]));
    assert_eq!(value["secret"], false);
    assert_eq!(value["input_tokens"], 123);
    let cmd = value["cmd"].as_str().expect("cmd");
    assert!(!cmd.contains("hunter2"), "{cmd}");
}

#[test]
fn token_shaped_keys_use_the_text_rules_in_both_json_paths() {
    let key = concat!("sk-live", "abcdefghijklmnopqrstuvwx");
    let url = "https://user:hunter2hunter2@example.invalid/x";
    let value = serde_json::json!({
        "env": {key: true, url: 1, "PATH": "/bin"},
        "dup": {"[redacted]": "first", key: "second"}
    });
    let mut redacted = value.clone();
    json(&mut redacted);
    let rendered = redacted.to_string();
    assert!(!rendered.contains("abcdefghijklmnop"), "{rendered}");
    assert!(!rendered.contains("hunter2"), "{rendered}");
    assert_eq!(redacted["env"]["[redacted]"], true);
    assert_eq!(
        redacted["env"]["https://user:[redacted]@example.invalid/x"],
        1
    );
    assert_eq!(redacted["env"]["PATH"], "/bin");
    assert_eq!(redacted["dup"], serde_json::json!({"[redacted]": "first"}));

    let (pretty, clipped) = pretty_bounded(&value, 1_000_000);
    assert!(!clipped);
    assert!(!pretty.contains("abcdefghijklmnop"), "{pretty}");
    assert!(!pretty.contains("hunter2"), "{pretty}");
    assert!(pretty.contains("\"[redacted]\": \"first\""), "{pretty}");
    assert!(!pretty.contains("second"), "{pretty}");
    let reparsed: Value = serde_json::from_str(&pretty).expect("pretty is JSON");
    assert_eq!(reparsed, redacted);
}

#[test]
fn pretty_bounded_matches_json_redaction_and_stops_without_the_tail() {
    let value = serde_json::json!({
        "cmd": "cargo test",
        "n": 1,
        "ok": true,
        "empty": {},
        "items": [null, "a\"b"],
        "api_key": {"v": "hunter2hunter2", "n": 3}
    });
    let mut redacted = value.clone();
    json(&mut redacted);
    let (pretty, clipped) = pretty_bounded(&value, 1_000_000);
    assert!(!clipped);
    assert_eq!(pretty, format!("{redacted:#}"));
    assert!(!pretty.contains("hunter2"));

    // The tail past the limit is never visited. Redacting each of these
    // strings is a pass of every text rule, so walking the whole array
    // would cost far more CPU than the bound; the clipped prefix is a few
    // hundred strings.
    let huge = serde_json::json!({"v": vec!["token=abcdefgh12345678"; 200_000]});
    let started = process_cpu_time();
    let (prefix, clipped) = pretty_bounded(&huge, 8 * 1024);
    let spent = process_cpu_time().saturating_sub(started);
    assert!(clipped);
    assert!(prefix.len() <= 8 * 1024);
    assert!(
        spent < std::time::Duration::from_millis(250),
        "bounded pretty of a long array used {spent:?} of CPU"
    );
}
