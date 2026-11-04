use serde_json::Value;

use super::*;

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
    let defs = document["$defs"].as_object().unwrap();
    let mut found = Vec::new();
    refs(&document, &mut found);
    assert_ne!(found.len(), 0);
    for target in found {
        let name = target.strip_prefix("#/$defs/").unwrap();
        assert!(defs.contains_key(name), "{target}");
    }
    let text = document.to_string();
    for frame in ["hello", "request", "welcome", "event", "snapshot", "blob"] {
        assert!(text.contains(&format!("\"{frame}\"")), "{frame}");
    }
}
