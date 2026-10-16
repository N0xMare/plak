use serde_json::json;

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn lower(schema: &Value) -> Result<Value, SchemaRefusal> {
    let Value::Object(schema) = schema else {
        return Err(SchemaRefusal::new("test schema is not an object"));
    };
    lower_input_schema(schema).map(|lowered| lowered.wire_document())
}

fn refusal(schema: &Value) -> String {
    match lower(schema) {
        Ok(lowered) => format!("unexpectedly lowered: {lowered}"),
        Err(refusal) => refusal.reason().to_owned(),
    }
}

#[test]
fn annotation_keywords_are_dropped_and_objects_closed() -> TestResult {
    let lowered = lower(&json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": "urn:x",
        "$comment": "c",
        "title": "Args",
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "format": "uri",
                "pattern": "^/",
                "default": "/",
                "examples": ["/a"],
                "readOnly": false,
                "writeOnly": false,
                "deprecated": false,
                "title": "Path",
                "description": "where"
            }
        },
        "required": ["path"]
    }))?;
    assert_eq!(
        lowered,
        json!({
            "title": "Args",
            "type": "object",
            "properties": {
                "path": {"type": "string", "title": "Path", "description": "where"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    );
    Ok(())
}

#[test]
fn open_objects_and_missing_members_are_tightened() -> TestResult {
    for additional in [json!(true), json!({}), Value::Null] {
        let mut schema = json!({"type": "object", "properties": {"a": {"type": "object"}}});
        if !additional.is_null() {
            schema["additionalProperties"] = additional;
        }
        assert_eq!(
            lower(&schema)?,
            json!({
                "type": "object",
                "properties": {
                    "a": {"type": "object", "properties": {}, "required": [], "additionalProperties": false}
                },
                "required": [],
                "additionalProperties": false
            })
        );
    }
    // An empty root (a tool with no arguments) lowers to the empty object.
    assert_eq!(
        lower(&json!({"type": "object"}))?,
        json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false})
    );
    Ok(())
}

#[test]
fn local_refs_are_inlined_with_sibling_annotations() -> TestResult {
    let lowered = lower(&json!({
        "type": "object",
        "$defs": {"Owner": {"type": "object", "properties": {"login": {"type": "string"}}, "required": ["login"]}},
        "properties": {
            "owner": {"$ref": "#/$defs/Owner", "description": "repo owner"},
            "other": {"$ref": "#/$defs/Owner"}
        },
        "required": ["owner"]
    }))?;
    let owner = json!({
        "type": "object",
        "properties": {"login": {"type": "string"}},
        "required": ["login"],
        "additionalProperties": false
    });
    let mut described = owner.clone();
    described["description"] = json!("repo owner");
    assert_eq!(lowered["properties"]["owner"], described);
    assert_eq!(lowered["properties"]["other"], owner);
    // `definitions` is the draft-07 spelling of the same thing.
    let draft7 = lower(&json!({
        "type": "object",
        "definitions": {"N": {"type": "integer"}},
        "properties": {"n": {"$ref": "#/definitions/N"}}
    }))?;
    assert_eq!(draft7["properties"]["n"], json!({"type": "integer"}));
    Ok(())
}

#[test]
fn pydantic_optionals_become_their_type_and_leave_required() -> TestResult {
    let lowered = lower(&json!({
        "type": "object",
        "properties": {
            "a": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null, "title": "A"},
            "b": {"oneOf": [{"type": "null"}, {"type": "integer"}]},
            "c": {"type": ["boolean", "null"]},
            "d": {"type": ["string", "null"], "enum": ["x", "y", null]},
            "e": {"type": "string"}
        },
        "required": ["a", "b", "c", "d", "e"]
    }))?;
    assert_eq!(
        lowered,
        json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "title": "A"},
                "b": {"type": "integer"},
                "c": {"type": "boolean"},
                "d": {"type": "string", "enum": ["x", "y"]},
                "e": {"type": "string"}
            },
            "required": ["e"],
            "additionalProperties": false
        })
    );
    Ok(())
}

#[test]
fn types_are_inferred_from_properties_and_homogeneous_enums() -> TestResult {
    let lowered = lower(&json!({
        "properties": {"mode": {"enum": ["fast", "slow"]}, "n": {"enum": [1, 2]}}
    }))?;
    assert_eq!(lowered["type"], json!("object"));
    assert_eq!(lowered["properties"]["mode"]["type"], json!("string"));
    assert_eq!(lowered["properties"]["n"]["type"], json!("integer"));
    assert_eq!(
        refusal(&json!({"type": "object", "properties": {"m": {"enum": ["a", 1]}}})),
        "untyped schema at /properties/m"
    );
    Ok(())
}

#[test]
fn each_unsupported_construct_is_refused_by_name() {
    let object = |property: Value| json!({"type": "object", "properties": {"p": property}});
    let cases = [
        (
            json!({"type": "object", "$defs": {"N": {"type": "object", "properties": {"next": {"$ref": "#/$defs/N"}}}}, "properties": {"n": {"$ref": "#/$defs/N"}}}),
            "recursive $ref #/$defs/N",
        ),
        (
            object(json!({"$ref": "https://example.com/schema"})),
            "$ref https://example.com/schema at /properties/p is not a local definition",
        ),
        (
            object(json!({"type": "object", "additionalProperties": {"type": "string"}})),
            "additionalProperties with a schema at /properties/p",
        ),
        (
            object(json!({"allOf": [{"type": "string"}]})),
            "allOf at /properties/p",
        ),
        (
            object(json!({"anyOf": [{"type": "string"}, {"type": "integer"}]})),
            "anyOf at /properties/p",
        ),
        (
            object(json!({"oneOf": [{"type": "string"}, {"type": "null"}, {"type": "integer"}]})),
            "oneOf at /properties/p",
        ),
        (
            object(json!({"type": ["string", "integer"]})),
            "type at /properties/p",
        ),
        (object(json!({"type": "null"})), "type at /properties/p"),
        (object(json!({"const": "x"})), "const at /properties/p"),
        (
            object(json!({"type": "array", "items": [{"type": "string"}]})),
            "tuple items at /properties/p",
        ),
        (
            object(json!({"type": "array"})),
            "array without items at /properties/p",
        ),
        (
            object(json!({"type": "integer", "multipleOf": 2})),
            "multipleOf at /properties/p",
        ),
        (
            object(json!({"$ref": "#/$defs/X", "minimum": 1})),
            "$ref #/$defs/X at /properties/p is not a local definition",
        ),
        (
            json!({"type": "string"}),
            "the root schema is not an object",
        ),
    ];
    for (schema, expected) in cases {
        assert_eq!(refusal(&schema), expected, "{schema}");
    }
}

#[test]
fn limits_beyond_v2_are_refused() {
    let wide = (0..65)
        .map(|index| (format!("p{index}"), json!({"type": "string"})))
        .collect::<Map<_, _>>();
    assert!(
        refusal(&json!({"type": "object", "properties": wide}))
            .starts_with("outside the portable schema limits")
    );
    let mut deep = json!({"type": "string"});
    for _ in 0..9 {
        deep = json!({"type": "object", "properties": {"d": deep}});
    }
    assert!(refusal(&deep).starts_with("outside the portable schema limits"));
    let many = (0..129)
        .map(|index| json!(format!("v{index}")))
        .collect::<Vec<_>>();
    assert!(
        refusal(&json!({"type": "object", "properties": {"e": {"type": "string", "enum": many}}}))
            .starts_with("outside the portable schema limits")
    );
}

#[test]
fn ref_siblings_other_than_annotations_are_refused() {
    let schema = json!({
        "type": "object",
        "$defs": {"X": {"type": "integer"}},
        "properties": {"p": {"$ref": "#/$defs/X", "minimum": 1}}
    });
    assert_eq!(refusal(&schema), "minimum beside $ref at /properties/p");
}
