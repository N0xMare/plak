//! Bounded optional-field tool schemas for exact-model projections.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::{Digest, DigestDomain, SemanticJsonError, SemanticJsonV1};

/// Portable projection schema revision implemented by this crate.
pub const PORTABLE_TOOL_SCHEMA_V2_REVISION: u32 = 2;
const PORTABLE_SCHEMA_MAX_BYTES: usize = 65_536;
const PORTABLE_SCHEMA_MAX_DEPTH: usize = 8;
const PORTABLE_SCHEMA_MAX_NODES: usize = 4_096;
const PORTABLE_SCHEMA_MAX_PROPERTIES_PER_OBJECT: usize = 64;
const PORTABLE_SCHEMA_MAX_PROPERTIES_TOTAL: usize = 256;
const PORTABLE_SCHEMA_MAX_ENUM_ENTRIES: usize = 128;
const PORTABLE_SCHEMA_MAX_VISIBLE_STRING_BYTES: usize = 32_768;
const PORTABLE_SCHEMA_MAX_STRING_BYTES: usize = 4_096;
const MAX_SAFE_INTEGER_F64: f64 = 9_007_199_254_740_991.0;
const DRAFT_2020_12_URI: &str = "https://json-schema.org/draft/2020-12/schema";

/// Bounded portable schema for exact-model tool projections.
///
/// V2 retains V1's closed-object, no-reference, no-combinator contract while
/// permitting a duplicate-free `required` subset and validating `integer`
/// separately from general `number`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PortableToolSchemaV2 {
    document: SemanticJsonV1,
    digest: Digest,
}

impl PortableToolSchemaV2 {
    /// Validates and freezes one exact V2 schema document.
    ///
    /// # Errors
    ///
    /// Returns a content-free error for semantic JSON outside the fixed bounds
    /// or a schema outside the closed V2 keyword and shape contract.
    pub fn new(document: Value) -> Result<Self, SemanticJsonError> {
        let document = SemanticJsonV1::from_value(document)?;
        if document.canonical_bytes().len() > PORTABLE_SCHEMA_MAX_BYTES {
            return Err(invalid_v2("schema byte budget exceeded"));
        }
        validate_v2_strings(document.value(), &mut 0)?;
        validate_v2_schema_node(document.value(), true, 0, &mut V2SchemaBudget::default())?;
        let digest = Digest::sha256(
            DigestDomain::ToolDefinition,
            PORTABLE_TOOL_SCHEMA_V2_REVISION,
            &[
                b"plakous-tools/PortableToolSchemaV2",
                document.digest().bytes(),
            ],
        )
        .map_err(|_| SemanticJsonError::Integrity)?;
        Ok(Self { document, digest })
    }

    /// Strictly parses and validates one structured instance.
    ///
    /// # Errors
    ///
    /// Returns a semantic JSON error or schema mismatch without repair.
    pub fn parse_instance(&self, raw: &str) -> Result<SemanticJsonV1, SemanticJsonError> {
        let value = SemanticJsonV1::parse(raw)?;
        self.validate(&value)?;
        Ok(value)
    }

    /// Validates an already admitted semantic value.
    ///
    /// # Errors
    ///
    /// Returns [`SemanticJsonError::SchemaMismatch`] for any mismatch.
    pub fn validate(&self, value: &SemanticJsonV1) -> Result<(), SemanticJsonError> {
        validate_v2_instance(self.document.value(), value.value())
    }

    /// Returns the exact bounded semantic schema document.
    #[must_use]
    pub const fn document(&self) -> &SemanticJsonV1 {
        &self.document
    }

    /// Returns the revision-bound V2 schema identity.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// Returns the model-visible projection of this schema.
    ///
    /// The durable document always carries the exact `$schema` dialect URI so
    /// the V2 contract and the schema digest stay stable. Providers infer that
    /// dialect, so the wire projection drops the key and nothing else; every
    /// other keyword, including `additionalProperties`, is preserved.
    #[must_use]
    pub fn wire_document(&self) -> Value {
        let mut document = self.document.value().as_ref().clone();
        if let Some(object) = document.as_object_mut() {
            object.remove("$schema");
        }
        document
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableSchemaV2Wire {
    document: SemanticJsonV1,
    digest: Digest,
}

impl<'de> Deserialize<'de> for PortableToolSchemaV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PortableSchemaV2Wire::deserialize(deserializer)?;
        let schema =
            Self::new(wire.document.value().as_ref().clone()).map_err(serde::de::Error::custom)?;
        if schema.digest != wire.digest {
            return Err(serde::de::Error::custom(SemanticJsonError::Integrity));
        }
        Ok(schema)
    }
}

#[derive(Default)]
struct V2SchemaBudget {
    nodes: usize,
    properties: usize,
}

#[expect(
    clippy::too_many_lines,
    reason = "the closed V2 keyword/type matrix is clearest in one authority"
)]
fn validate_v2_schema_node(
    schema: &Value,
    root: bool,
    depth: usize,
    budget: &mut V2SchemaBudget,
) -> Result<(), SemanticJsonError> {
    if depth > PORTABLE_SCHEMA_MAX_DEPTH {
        return Err(invalid_v2("schema depth exceeds V2"));
    }
    budget.nodes = budget.nodes.saturating_add(1);
    if budget.nodes > PORTABLE_SCHEMA_MAX_NODES {
        return Err(invalid_v2("schema node count exceeds V2"));
    }
    let object = schema
        .as_object()
        .ok_or_else(|| invalid_v2("schema nodes must be objects"))?;
    let allowed = [
        "$schema",
        "title",
        "description",
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid_v2("unsupported keyword"));
    }
    if !root && object.contains_key("$schema") {
        return Err(invalid_v2("$schema is root-only"));
    }
    if root && object.get("$schema").and_then(Value::as_str) != Some(DRAFT_2020_12_URI) {
        return Err(invalid_v2("the exact root dialect URI is required"));
    }
    for field in ["title", "description"] {
        if object.contains_key(field) && object.get(field).and_then(Value::as_str).is_none() {
            return Err(invalid_v2("title and description must be strings"));
        }
    }
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_v2("type is required"))?;
    if root && kind != "object" {
        return Err(invalid_v2("schema root must be object"));
    }
    if !matches!(
        kind,
        "object" | "array" | "string" | "number" | "integer" | "boolean"
    ) {
        return Err(invalid_v2("unsupported type"));
    }
    match kind {
        "object" => validate_v2_object(object, depth, budget)?,
        "array" => {
            reject_v2_keywords(
                object,
                &[
                    "properties",
                    "required",
                    "additionalProperties",
                    "enum",
                    "minimum",
                    "maximum",
                    "minLength",
                    "maxLength",
                ],
            )?;
            validate_v2_unsigned_bounds(object, "minItems", "maxItems")?;
            validate_v2_schema_node(
                object
                    .get("items")
                    .ok_or_else(|| invalid_v2("array items are required"))?,
                false,
                depth.saturating_add(1),
                budget,
            )?;
        }
        "string" => {
            reject_v2_keywords(
                object,
                &[
                    "properties",
                    "required",
                    "additionalProperties",
                    "items",
                    "minimum",
                    "maximum",
                    "minItems",
                    "maxItems",
                ],
            )?;
            validate_v2_unsigned_bounds(object, "minLength", "maxLength")?;
        }
        "number" | "integer" => {
            reject_v2_keywords(
                object,
                &[
                    "properties",
                    "required",
                    "additionalProperties",
                    "items",
                    "minLength",
                    "maxLength",
                    "minItems",
                    "maxItems",
                ],
            )?;
            validate_v2_numeric_bounds(object, kind)?;
        }
        "boolean" => reject_v2_keywords(
            object,
            &[
                "properties",
                "required",
                "additionalProperties",
                "items",
                "minimum",
                "maximum",
                "minLength",
                "maxLength",
                "minItems",
                "maxItems",
            ],
        )?,
        _ => unreachable!("kind is closed above"),
    }
    validate_v2_enum(object, kind)
}

fn validate_v2_object(
    object: &Map<String, Value>,
    depth: usize,
    budget: &mut V2SchemaBudget,
) -> Result<(), SemanticJsonError> {
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_v2("object properties are required"))?;
    if properties.len() > PORTABLE_SCHEMA_MAX_PROPERTIES_PER_OBJECT {
        return Err(invalid_v2("too many object properties"));
    }
    budget.properties = budget.properties.saturating_add(properties.len());
    if budget.properties > PORTABLE_SCHEMA_MAX_PROPERTIES_TOTAL {
        return Err(invalid_v2("too many total properties"));
    }
    if object.get("additionalProperties") != Some(&Value::Bool(false)) {
        return Err(invalid_v2("objects must be closed"));
    }
    let required = object
        .get("required")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_v2("required must be an array"))?;
    let mut names = HashSet::with_capacity(required.len());
    for name in required {
        let name = name
            .as_str()
            .ok_or_else(|| invalid_v2("required names must be strings"))?;
        if !properties.contains_key(name) || !names.insert(name) {
            return Err(invalid_v2(
                "required contains an unknown or duplicate property",
            ));
        }
    }
    for child in properties.values() {
        validate_v2_schema_node(child, false, depth.saturating_add(1), budget)?;
    }
    reject_v2_keywords(
        object,
        &[
            "items",
            "enum",
            "minimum",
            "maximum",
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
        ],
    )
}

fn validate_v2_unsigned_bounds(
    object: &Map<String, Value>,
    minimum: &str,
    maximum: &str,
) -> Result<(), SemanticJsonError> {
    let parse = |field| {
        object
            .get(field)
            .map(|value| {
                value
                    .as_f64()
                    .filter(|value| {
                        value.is_finite()
                            && *value >= 0.0
                            && value.fract() == 0.0
                            && *value <= MAX_SAFE_INTEGER_F64
                    })
                    .and_then(|value| value.to_string().parse::<usize>().ok())
                    .ok_or_else(|| invalid_v2("length bounds must be unsigned integers"))
            })
            .transpose()
    };
    let lower = parse(minimum)?;
    let upper = parse(maximum)?;
    if lower.zip(upper).is_some_and(|(lower, upper)| lower > upper) {
        return Err(invalid_v2("minimum bound exceeds maximum"));
    }
    Ok(())
}

fn validate_v2_numeric_bounds(
    object: &Map<String, Value>,
    kind: &str,
) -> Result<(), SemanticJsonError> {
    let parse = |field| {
        object
            .get(field)
            .map(|value| {
                if !v2_instance_type_matches(kind, value) {
                    return Err(invalid_v2("numeric bound does not match its type"));
                }
                value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| invalid_v2("numeric bound is unsafe"))
            })
            .transpose()
    };
    let lower = parse("minimum")?;
    let upper = parse("maximum")?;
    if lower.zip(upper).is_some_and(|(lower, upper)| lower > upper) {
        return Err(invalid_v2("minimum bound exceeds maximum"));
    }
    Ok(())
}

fn validate_v2_enum(object: &Map<String, Value>, kind: &str) -> Result<(), SemanticJsonError> {
    let Some(values) = object.get("enum") else {
        return Ok(());
    };
    let values = values
        .as_array()
        .ok_or_else(|| invalid_v2("enum must be an array"))?;
    if values.is_empty() || values.len() > PORTABLE_SCHEMA_MAX_ENUM_ENTRIES {
        return Err(invalid_v2("enum size is outside V2"));
    }
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if !v2_instance_type_matches(kind, value) {
            return Err(invalid_v2("enum values must match the scalar type"));
        }
        let semantic = SemanticJsonV1::from_value(value.clone())?;
        if !seen.insert(Arc::clone(semantic.canonical_bytes())) {
            return Err(invalid_v2("enum contains a duplicate"));
        }
    }
    Ok(())
}

fn reject_v2_keywords(
    object: &Map<String, Value>,
    forbidden: &[&str],
) -> Result<(), SemanticJsonError> {
    if forbidden.iter().any(|key| object.contains_key(*key)) {
        return Err(invalid_v2("keyword does not apply to type"));
    }
    Ok(())
}

fn validate_v2_instance(schema: &Value, value: &Value) -> Result<(), SemanticJsonError> {
    let object = schema
        .as_object()
        .ok_or(SemanticJsonError::SchemaMismatch)?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(SemanticJsonError::SchemaMismatch)?;
    if !v2_instance_type_matches(kind, value) {
        return Err(SemanticJsonError::SchemaMismatch);
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        return Err(SemanticJsonError::SchemaMismatch);
    }
    match (kind, value) {
        ("object", Value::Object(instance)) => {
            let properties = object
                .get("properties")
                .and_then(Value::as_object)
                .ok_or(SemanticJsonError::SchemaMismatch)?;
            if instance.keys().any(|name| !properties.contains_key(name)) {
                return Err(SemanticJsonError::SchemaMismatch);
            }
            for required in object
                .get("required")
                .and_then(Value::as_array)
                .ok_or(SemanticJsonError::SchemaMismatch)?
            {
                if !instance
                    .contains_key(required.as_str().ok_or(SemanticJsonError::SchemaMismatch)?)
                {
                    return Err(SemanticJsonError::SchemaMismatch);
                }
            }
            for (name, child) in instance {
                validate_v2_instance(
                    properties
                        .get(name)
                        .ok_or(SemanticJsonError::SchemaMismatch)?,
                    child,
                )?;
            }
        }
        ("array", Value::Array(values)) => {
            validate_v2_instance_length(object, values.len(), "minItems", "maxItems")?;
            for child in values {
                validate_v2_instance(
                    object
                        .get("items")
                        .ok_or(SemanticJsonError::SchemaMismatch)?,
                    child,
                )?;
            }
        }
        ("string", Value::String(text)) => {
            validate_v2_instance_length(object, text.chars().count(), "minLength", "maxLength")?;
        }
        ("number" | "integer", Value::Number(number)) => {
            let number = number.as_f64().ok_or(SemanticJsonError::SchemaMismatch)?;
            if object
                .get("minimum")
                .and_then(Value::as_f64)
                .is_some_and(|minimum| number < minimum)
                || object
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .is_some_and(|maximum| number > maximum)
            {
                return Err(SemanticJsonError::SchemaMismatch);
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_v2_instance_length(
    schema: &Map<String, Value>,
    length: usize,
    minimum: &str,
    maximum: &str,
) -> Result<(), SemanticJsonError> {
    if schema
        .get(minimum)
        .and_then(v2_length_bound)
        .is_some_and(|minimum| length < minimum)
        || schema
            .get(maximum)
            .and_then(v2_length_bound)
            .is_some_and(|maximum| length > maximum)
    {
        return Err(SemanticJsonError::SchemaMismatch);
    }
    Ok(())
}

fn v2_length_bound(value: &Value) -> Option<usize> {
    value.as_f64().and_then(|value| {
        (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= MAX_SAFE_INTEGER_F64)
            .then(|| value.to_string().parse().ok())
            .flatten()
    })
}

fn v2_instance_type_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.as_f64().is_some_and(f64::is_finite),
        "integer" => value.as_f64().is_some_and(|number| {
            number.is_finite() && number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER_F64
        }),
        "boolean" => value.is_boolean(),
        _ => false,
    }
}

fn validate_v2_strings(value: &Value, total: &mut usize) -> Result<(), SemanticJsonError> {
    match value {
        Value::String(text) => {
            if text.len() > PORTABLE_SCHEMA_MAX_STRING_BYTES {
                return Err(invalid_v2("schema string exceeds V2"));
            }
            *total = total.saturating_add(text.len());
            if *total > PORTABLE_SCHEMA_MAX_VISIBLE_STRING_BYTES {
                return Err(invalid_v2("schema string budget exceeded"));
            }
        }
        Value::Array(values) => {
            for child in values {
                validate_v2_strings(child, total)?;
            }
        }
        Value::Object(values) => {
            for (key, child) in values {
                validate_v2_strings(&Value::String(key.clone()), total)?;
                validate_v2_strings(child, total)?;
            }
        }
        _ => {}
    }
    Ok(())
}

const fn invalid_v2(message: &'static str) -> SemanticJsonError {
    SemanticJsonError::InvalidSchema(message)
}

#[cfg(test)]
mod tests {
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
            PortableToolSchemaV2::new(v2_root(Map::from_iter([("x".into(), child)]), vec![],))
                .is_err()
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
}
