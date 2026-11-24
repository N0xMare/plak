use serde_json::Value;

use super::*;

#[test]
fn protocol_twelve_schema_has_an_exact_build_independent_pin() {
    assert_eq!(
        pin().unwrap(),
        "378712bb946ce471bb589fc05fa29f31764726c6e5581b87f89d5c1cb864b2ac"
    );
}

#[test]
fn protocol_thirteen_schema_has_an_exact_build_independent_pin() {
    let mut output = Vec::new();
    print(&mut output, true).unwrap();
    assert!(
        output.as_slice()
            == include_bytes!("../../tests/fixtures/protocol-13-wire-schema.json").as_slice(),
        "regenerate the artifact with plakd wire schema --protocol 13",
    );
    let document: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(document["wire_version"], v13::WIRE_VERSION);
    assert_eq!(
        normalized_pin(document).unwrap(),
        "57c64dba0df2ac898699e8d401e89240d820927957c2bb0101302d891b1e4f42",
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
    assert_eq!(document["wire_version"], Value::from(WIRE_VERSION));
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
fn the_draft_schema_is_self_contained_and_does_not_advertise_replaced_cases() {
    let document = draft_schema();
    assert_self_contained(&document);
    let text = document.to_string();
    for absent in [
        "assistant_delta",
        "reasoning_delta",
        "presentation_snapshot",
        "needs_operator",
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
    ] {
        assert!(text.contains(&format!("\"{present}\"")), "{present}");
    }
}
