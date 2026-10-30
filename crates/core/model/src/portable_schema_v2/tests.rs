use super::*;
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[expect(
    clippy::needless_pass_by_value,
    reason = "the JSON macro consumes the owned properties and required values"
)]
fn v2_root(properties: Map<String, Value>, required: Vec<Value>) -> Value {
    json!({
        "$schema": DRAFT_2020_12_URI,
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

#[test]
fn portable_v2_accepts_optional_closed_properties_and_validates_presence() -> TestResult {
    let schema = PortableToolSchemaV2::new(v2_root(
        Map::from_iter([
            ("required".into(), json!({"type":"boolean"})),
            ("optional".into(), json!({"type":"string"})),
        ]),
        vec![json!("required")],
    ))?;

    schema.parse_instance(r#"{"required":true}"#)?;
    schema.parse_instance(r#"{"required":true,"optional":"x"}"#)?;
    assert!(schema.parse_instance(r#"{"optional":"x"}"#).is_err());
    assert!(
        schema
            .parse_instance(r#"{"required":true,"unknown":false}"#)
            .is_err()
    );
    Ok(())
}

#[test]
fn portable_v2_required_is_a_duplicate_free_property_subset() {
    let properties = Map::from_iter([
        ("a".into(), json!({"type":"string"})),
        ("b".into(), json!({"type":"boolean"})),
    ]);
    assert!(PortableToolSchemaV2::new(v2_root(properties.clone(), vec![])).is_ok());
    assert!(
        PortableToolSchemaV2::new(v2_root(properties.clone(), vec![json!("a"), json!("a")]))
            .is_err()
    );
    assert!(PortableToolSchemaV2::new(v2_root(properties, vec![json!("unknown")])).is_err());
}

#[test]
fn portable_v2_integer_is_exact_while_general_number_accepts_both_forms() -> TestResult {
    let schema = PortableToolSchemaV2::new(v2_root(
        Map::from_iter([
            ("integer".into(), json!({"type":"integer"})),
            ("number".into(), json!({"type":"number"})),
        ]),
        vec![json!("integer"), json!("number")],
    ))?;
    schema.parse_instance(r#"{"integer":2,"number":2.5}"#)?;
    schema.parse_instance(r#"{"integer":2,"number":2}"#)?;
    assert!(
        schema
            .parse_instance(r#"{"integer":2.5,"number":2.5}"#)
            .is_err()
    );
    assert!(
        schema
            .parse_instance(r#"{"integer":9007199254740992,"number":2.5}"#)
            .is_err()
    );
    assert!(
        PortableToolSchemaV2::new(v2_root(
            Map::from_iter([(
                "integer".into(),
                json!({"type":"integer","enum":[9_007_199_254_740_992_u64]})
            )]),
            vec![],
        ))
        .is_err()
    );
    Ok(())
}

#[test]
fn portable_v2_accepts_only_the_projection_keyword_matrix() {
    for child in [
        json!({"type":["string","null"]}),
        json!({"type":"string","default":"x"}),
        json!({"type":"string","pattern":"x"}),
        json!({"type":"number","exclusiveMinimum":0}),
        json!({"type":"string","$ref":"#/$defs/x"}),
        json!({"type":"string","oneOf":[{"type":"string"}]}),
        json!({"type":"array","items":true}),
        json!({"type":"array"}),
        json!({"type":"object","properties":{},"required":[],"additionalProperties":true}),
    ] {
        assert!(
            PortableToolSchemaV2::new(v2_root(Map::from_iter([("x".into(), child)]), vec![]))
                .is_err()
        );
    }
    assert!(PortableToolSchemaV2::new(json!(true)).is_err());
    assert!(
        PortableToolSchemaV2::new(json!({
            "$schema": DRAFT_2020_12_URI,
            "type": "string"
        }))
        .is_err()
    );
}

#[test]
fn portable_v2_enforces_only_the_approved_scalar_and_collection_bounds() -> TestResult {
    let schema = PortableToolSchemaV2::new(v2_root(
        Map::from_iter([
            (
                "text".into(),
                json!({"type":"string","minLength":1,"maxLength":2}),
            ),
            (
                "values".into(),
                json!({
                    "type":"array",
                    "minItems":1,
                    "maxItems":2,
                    "items":{"type":"integer","minimum":-2,"maximum":2}
                }),
            ),
        ]),
        vec![json!("text"), json!("values")],
    ))?;
    schema.parse_instance(r#"{"text":"éx","values":[-2,2]}"#)?;
    for invalid in [
        r#"{"text":"","values":[0]}"#,
        r#"{"text":"abc","values":[0]}"#,
        r#"{"text":"x","values":[]}"#,
        r#"{"text":"x","values":[0,1,2]}"#,
        r#"{"text":"x","values":[3]}"#,
    ] {
        assert!(schema.parse_instance(invalid).is_err(), "{invalid}");
    }

    for invalid_bound in [
        json!({"type":"string","minLength":2,"maxLength":1}),
        json!({"type":"array","minItems":-1,"items":{"type":"boolean"}}),
        json!({"type":"integer","minimum":1.5}),
        json!({"type":"integer","maximum":9_007_199_254_740_992_u64}),
        json!({"type":"number","minimum":2,"maximum":1}),
    ] {
        assert!(
            PortableToolSchemaV2::new(v2_root(
                Map::from_iter([("x".into(), invalid_bound)]),
                vec![],
            ))
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn portable_v2_retains_fixed_width_depth_enum_and_string_bounds() {
    let properties = (0..=PORTABLE_SCHEMA_MAX_PROPERTIES_PER_OBJECT)
        .map(|index| (format!("p{index}"), json!({"type":"boolean"})))
        .collect();
    assert!(PortableToolSchemaV2::new(v2_root(properties, vec![])).is_err());

    let values = (0..=PORTABLE_SCHEMA_MAX_ENUM_ENTRIES)
        .map(|index| json!(format!("v{index}")))
        .collect::<Vec<_>>();
    assert!(
        PortableToolSchemaV2::new(v2_root(
            Map::from_iter([("x".into(), json!({"type":"string","enum":values}))]),
            vec![],
        ))
        .is_err()
    );

    let mut child = json!({"type":"boolean"});
    for _ in 0..=PORTABLE_SCHEMA_MAX_DEPTH {
        child = json!({"type":"array","items":child});
    }
    assert!(
        PortableToolSchemaV2::new(v2_root(Map::from_iter([("x".into(), child)]), vec![],)).is_err()
    );

    assert!(
        PortableToolSchemaV2::new(v2_root(
            Map::from_iter([(
                "x".into(),
                json!({
                    "type":"string",
                    "description":"x".repeat(PORTABLE_SCHEMA_MAX_STRING_BYTES + 1)
                })
            )]),
            vec![],
        ))
        .is_err()
    );
}

#[test]
fn portable_v2_serialization_revalidates_and_binds_revision_identity() -> TestResult {
    let schema = PortableToolSchemaV2::new(v2_root(
        Map::from_iter([("optional".into(), json!({"type":"integer"}))]),
        vec![],
    ))?;
    let encoded = serde_json::to_value(&schema)?;
    assert_eq!(
        serde_json::from_value::<PortableToolSchemaV2>(encoded.clone())?,
        schema
    );
    let mut tampered = encoded;
    tampered["digest"] = serde_json::to_value(Digest::sha256(
        DigestDomain::ToolDefinition,
        1,
        &[b"wrong"],
    )?)?;
    assert!(serde_json::from_value::<PortableToolSchemaV2>(tampered).is_err());
    Ok(())
}
