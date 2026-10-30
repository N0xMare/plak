use super::*;
use serde_json::json;

fn object(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    }
}

#[test]
fn required_value_missing_and_present() {
    let fixture = json!({"a": 1});
    let map = object(&fixture);
    assert_eq!(
        required_value(&map, "b"),
        Err(ResponsesFieldError::Missing { field: "b" })
    );
    assert_eq!(
        required_value(&map, "a").ok().and_then(Value::as_u64),
        Some(1)
    );
}

#[test]
fn required_string_and_object_type_checks() {
    let fixture = json!({"s": "ok", "n": 1, "o": {"k": true}});
    let map = object(&fixture);
    assert_eq!(required_string(&map, "s"), Ok("ok"));
    assert_eq!(
        required_string(&map, "n"),
        Err(ResponsesFieldError::Invalid { field: "n" })
    );
    assert_eq!(
        required_string(&map, "missing"),
        Err(ResponsesFieldError::Missing { field: "missing" })
    );
    assert!(required_object(&map, "o").is_ok());
    assert_eq!(
        required_object(&map, "s"),
        Err(ResponsesFieldError::Invalid { field: "s" })
    );
}

#[test]
fn optional_string_null_absent_and_wrong_type() {
    let fixture = json!({"a": "x", "n": null, "bad": 3});
    let map = object(&fixture);
    assert_eq!(optional_string(&map, "a"), Ok(Some("x")));
    assert_eq!(optional_string(&map, "n"), Ok(None));
    assert_eq!(optional_string(&map, "absent"), Ok(None));
    assert_eq!(
        optional_string(&map, "bad"),
        Err(ResponsesFieldError::Invalid { field: "bad" })
    );
}

#[test]
fn required_u32_and_u64() {
    let fixture = json!({"i": 7u64, "big": u64::from(u32::MAX) + 1, "s": "no"});
    let map = object(&fixture);
    assert_eq!(required_u32(&map, "i"), Ok(7));
    assert_eq!(required_u64(&map, "i"), Ok(7));
    assert_eq!(
        required_u32(&map, "big"),
        Err(ResponsesFieldError::Invalid { field: "big" })
    );
    assert_eq!(required_u64(&map, "big"), Ok(u64::from(u32::MAX) + 1));
    assert_eq!(
        required_u64(&map, "s"),
        Err(ResponsesFieldError::Invalid { field: "s" })
    );
    assert_eq!(
        required_u32(&map, "missing"),
        Err(ResponsesFieldError::Missing { field: "missing" })
    );
}

#[test]
fn optional_u64_fields_try_order_and_dual_conflict() {
    let fields = &["sequence_number", "sequence"];
    let only_primary = object(&json!({"sequence_number": 3}));
    assert_eq!(optional_u64_fields(&only_primary, fields), Ok(Some(3)));

    let only_alias = object(&json!({"sequence": 9}));
    assert_eq!(optional_u64_fields(&only_alias, fields), Ok(Some(9)));

    let neither = object(&json!({"type": "response.created"}));
    assert_eq!(optional_u64_fields(&neither, fields), Ok(None));

    let both = object(&json!({"sequence_number": 1, "sequence": 1}));
    assert_eq!(
        optional_u64_fields(&both, fields),
        Err(ResponsesFieldError::Invalid {
            field: "sequence_number"
        })
    );

    let bad_type = object(&json!({"sequence_number": "nope"}));
    assert_eq!(
        optional_u64_fields(&bad_type, fields),
        Err(ResponsesFieldError::Invalid {
            field: "sequence_number"
        })
    );
}

#[test]
fn optional_u64_fields_single_list_ignores_unlisted_keys() {
    // OpenRouter-style: only sequence_number is listed; bare `sequence` is a
    // dialect pre-check, not dual-field conflict among listed keys.
    let fields = &["sequence_number"];
    let alias_only = object(&json!({"sequence": 1}));
    assert_eq!(optional_u64_fields(&alias_only, fields), Ok(None));
    let primary = object(&json!({"sequence_number": 2, "sequence": 9}));
    assert_eq!(optional_u64_fields(&primary, fields), Ok(Some(2)));
}
