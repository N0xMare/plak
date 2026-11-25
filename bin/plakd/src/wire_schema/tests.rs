use serde_json::Value;

use super::*;

#[test]
fn protocol_thirteen_schema_has_an_exact_build_independent_pin() {
    let document = schema(None);
    let output = serde_json::to_string_pretty(&document).unwrap() + "\n";
    assert!(
        output.as_bytes()
            == include_bytes!("../../tests/fixtures/protocol-13-wire-schema.json").as_slice(),
        "the served schema differs from the reviewed artifact",
    );
    assert_eq!(document["wire_version"], v13::WIRE_VERSION);
    assert_eq!(
        pin().unwrap(),
        "2274665a5932cb62bba1fed44e06e89009e0f7a596123033b1c7c3e2acdaffc4",
    );
}

fn refs<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("$ref", Value::String(target)) => found.push(target),
                    _ => refs(value, found),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| refs(item, found)),
        _ => {}
    }
}

/// The schema names this build's protocol version, and every reference in
/// it resolves inside the document: both directions are complete.
#[test]
fn the_schema_is_versioned_and_self_contained() {
    let document = document();
    assert_eq!(document["wire_version"], Value::from(v13::WIRE_VERSION));
    assert_eq!(document["binary_id"], crate::build_identity::binary_id());
    assert_self_contained(&document);
    let text = document.to_string();
    for frame in ["hello", "request", "welcome", "event", "snapshot", "blob"] {
        assert!(text.contains(&format!("\"{frame}\"")), "{frame}");
    }
}

fn assert_self_contained(document: &Value) {
    let defs = document["$defs"].as_object().unwrap();
    let mut found = Vec::new();
    refs(document, &mut found);
    assert_ne!(found.len(), 0);
    for target in found {
        let name = target.strip_prefix("#/$defs/").unwrap();
        assert!(defs.contains_key(name), "{target}");
    }
}

#[test]
fn the_served_schema_does_not_advertise_replaced_cases() {
    let document = schema(None);
    assert_self_contained(&document);
    let text = document.to_string();
    for absent in [
        "assistant_delta",
        "reasoning_delta",
        "presentation_snapshot",
        "needs_operator",
        "fast_mode",
        "root_fast",
    ] {
        assert!(!text.contains(&format!("\"{absent}\"")), "{absent}");
    }
    for present in [
        "retained",
        "history_page",
        "truncated_bytes",
        "transcript_stream",
        "live_rows",
        "attachment_status",
        "interrupted_by_upgrade",
        "remaining_micros",
        "blob_refused",
        "seconds_remaining",
        "cursor_expired",
        "vcs_unsupported_version",
        "service_tier",
        "ultrafast",
    ] {
        assert!(text.contains(&format!("\"{present}\"")), "{present}");
    }
}
