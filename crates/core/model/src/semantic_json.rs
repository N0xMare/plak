//! One bounded semantic JSON and portable tool-schema authority.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{Digest, DigestDomain};

/// Semantic JSON codec revision implemented by this release.
pub const SEMANTIC_JSON_REVISION: u32 = 1;
/// Portable tool-schema revision implemented by this release.
pub const PORTABLE_TOOL_SCHEMA_REVISION: u32 = 1;

const MAX_BYTES: usize = 1_048_576;
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 32_768;
const MAX_STRING_BYTES: usize = 262_144;
const MAX_SCHEMA_BYTES: usize = 65_536;
const MAX_SCHEMA_DEPTH: usize = 8;
const MAX_SCHEMA_NODES: usize = 4_096;
const MAX_PROPERTIES_PER_OBJECT: usize = 64;
const MAX_PROPERTIES_TOTAL: usize = 256;
const MAX_ENUM_ENTRIES: usize = 128;
const MAX_MODEL_VISIBLE_STRING_BYTES: usize = 32_768;
const MAX_SCHEMA_STRING_BYTES: usize = 4_096;
const SCHEMA_URI: &str = "https://json-schema.org/draft/2020-12/schema";
/// Exclusive of 2^53. JSON Schema integer keywords and I-JSON/JCS int53
/// values must serialize without a fraction, or xAI Responses rejects them.
const MAX_SAFE_INTEGER_F64: f64 = 9_007_199_254_740_991.0;

/// Fail-closed semantic JSON or portable-schema construction error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SemanticJsonError {
    /// Input was not one strict JSON value.
    #[error("semantic JSON is invalid")]
    Invalid,
    /// An object contained a duplicate decoded member name.
    #[error("semantic JSON contains a duplicate object key")]
    DuplicateKey,
    /// Input exceeded a fixed resource bound.
    #[error("semantic JSON exceeds its resource bounds")]
    ResourceLimit,
    /// A number loses lexical meaning when admitted as binary64.
    #[error("semantic JSON number is outside the portable numeric contract")]
    Numeric,
    /// A schema uses meaning outside `PortableToolSchemaV1`.
    #[error("portable tool schema is invalid: {0}")]
    InvalidSchema(&'static str),
    /// A value does not satisfy a portable schema.
    #[error("semantic JSON does not satisfy the portable schema")]
    SchemaMismatch,
    /// Integrity reconstruction did not reproduce the stored identity.
    #[error("semantic JSON integrity mismatch")]
    Integrity,
}

/// One model-owned strict semantic JSON value and its JCS-compatible bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct SemanticJsonV1 {
    value: Arc<Value>,
    canonical: Arc<[u8]>,
    digest: Digest,
}

impl SemanticJsonV1 {
    /// Maximum UTF-8 bytes in one decoded canonical JSON string.
    #[must_use]
    pub const fn maximum_string_bytes() -> usize {
        MAX_STRING_BYTES
    }

    /// Strictly parses exactly one bounded UTF-8 JSON value.
    ///
    /// # Errors
    ///
    /// Returns a content-free error for malformed, non-I-JSON, or over-bound input.
    pub fn parse(raw: &str) -> Result<Self, SemanticJsonError> {
        if raw.len() > MAX_BYTES {
            return Err(SemanticJsonError::ResourceLimit);
        }
        validate_number_lexemes(raw)?;
        let state = Rc::new(RefCell::new(ParseState {
            nodes: 0,
            error: None,
        }));
        let mut deserializer = serde_json::Deserializer::from_str(raw);
        let value = StrictSeed {
            state: Rc::clone(&state),
            depth: 0,
        }
        .deserialize(&mut deserializer)
        .map_err(|_| {
            state
                .borrow()
                .error
                .clone()
                .unwrap_or(SemanticJsonError::Invalid)
        })?;
        deserializer.end().map_err(|_| SemanticJsonError::Invalid)?;
        Self::from_value(value)
    }

    /// Constructs semantic JSON from an already parsed value.
    ///
    /// # Errors
    ///
    /// Returns a content-free error for non-I-JSON or over-bound input.
    pub fn from_value(value: Value) -> Result<Self, SemanticJsonError> {
        let value = normalize_numbers(value)?;
        validate_value_bounds(&value, 0, &mut 0)?;
        let canonical = canonicalize(&value)?;
        let digest = Digest::sha256(
            DigestDomain::ToolDefinition,
            SEMANTIC_JSON_REVISION,
            &[b"plakous-model/SemanticJsonV1", &canonical],
        )
        .map_err(|_| SemanticJsonError::Integrity)?;
        Ok(Self {
            value: Arc::new(value),
            canonical: Arc::from(canonical),
            digest,
        })
    }

    /// Returns the parsed semantic value.
    #[must_use]
    pub const fn value(&self) -> &Arc<Value> {
        &self.value
    }
    /// Returns exact JCS-compatible UTF-8 bytes.
    #[must_use]
    pub const fn canonical_bytes(&self) -> &Arc<[u8]> {
        &self.canonical
    }
    /// Returns the revision-bound semantic identity.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

impl fmt::Debug for SemanticJsonV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemanticJsonV1")
            .field("bytes", &self.canonical.len())
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

impl Serialize for SemanticJsonV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SemanticJsonWire {
            revision: SEMANTIC_JSON_REVISION,
            canonical: self.canonical.as_ref(),
            digest: self.digest,
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
struct SemanticJsonWire<'a> {
    revision: u32,
    canonical: &'a [u8],
    digest: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SemanticJsonOwnedWire {
    revision: u32,
    canonical: Vec<u8>,
    digest: Digest,
}

impl<'de> Deserialize<'de> for SemanticJsonV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SemanticJsonOwnedWire::deserialize(deserializer)?;
        if wire.revision != SEMANTIC_JSON_REVISION {
            return Err(serde::de::Error::custom(SemanticJsonError::Integrity));
        }
        let raw = std::str::from_utf8(&wire.canonical)
            .map_err(|_| serde::de::Error::custom(SemanticJsonError::Invalid))?;
        let value = Self::parse(raw).map_err(serde::de::Error::custom)?;
        if value.canonical.as_ref() != wire.canonical || value.digest != wire.digest {
            return Err(serde::de::Error::custom(SemanticJsonError::Integrity));
        }
        Ok(value)
    }
}

/// Fully validated conservative Draft 2020-12 subset for local tools.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PortableToolSchemaV1 {
    document: SemanticJsonV1,
    digest: Digest,
}

impl PortableToolSchemaV1 {
    /// Validates and freezes one portable structured contract.
    ///
    /// # Errors
    ///
    /// Returns a content-free error when the document is outside the exact V1 contract.
    pub fn new(document: Value) -> Result<Self, SemanticJsonError> {
        let document = SemanticJsonV1::from_value(document)?;
        if document.canonical_bytes().len() > MAX_SCHEMA_BYTES {
            return Err(SemanticJsonError::InvalidSchema(
                "schema byte budget exceeded",
            ));
        }
        let mut string_bytes = 0;
        validate_schema_strings(document.value(), &mut string_bytes)?;
        let mut budget = SchemaBudget::default();
        validate_schema_node(document.value(), true, 0, &mut budget)?;
        let digest = Digest::sha256(
            DigestDomain::ToolDefinition,
            PORTABLE_TOOL_SCHEMA_REVISION,
            &[
                b"plakous-model/PortableToolSchemaV1",
                document.digest().bytes(),
            ],
        )
        .map_err(|_| SemanticJsonError::Integrity)?;
        Ok(Self { document, digest })
    }

    /// Strictly parses and validates a structured instance.
    ///
    /// # Errors
    ///
    /// Returns a content-free parse, resource, numeric, or schema-mismatch error.
    pub fn parse_instance(&self, raw: &str) -> Result<SemanticJsonV1, SemanticJsonError> {
        let value = SemanticJsonV1::parse(raw)?;
        self.validate(&value)?;
        Ok(value)
    }

    /// Validates an already admitted semantic value.
    ///
    /// # Errors
    ///
    /// Returns [`SemanticJsonError::SchemaMismatch`] when validation fails.
    pub fn validate(&self, value: &SemanticJsonV1) -> Result<(), SemanticJsonError> {
        validate_instance(self.document.value(), value.value())
    }

    /// Returns the exact semantic schema document.
    #[must_use]
    pub const fn document(&self) -> &SemanticJsonV1 {
        &self.document
    }
    /// Returns the revision-bound schema identity.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableSchemaWire {
    document: SemanticJsonV1,
    digest: Digest,
}

impl<'de> Deserialize<'de> for PortableToolSchemaV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PortableSchemaWire::deserialize(deserializer)?;
        let schema =
            Self::new(wire.document.value().as_ref().clone()).map_err(serde::de::Error::custom)?;
        if schema.digest != wire.digest {
            return Err(serde::de::Error::custom(SemanticJsonError::Integrity));
        }
        Ok(schema)
    }
}

#[derive(Default)]
struct SchemaBudget {
    nodes: usize,
    properties: usize,
}

#[expect(
    clippy::too_many_lines,
    reason = "closed V1 keyword matrix is clearest in one recursive authority"
)]
fn validate_schema_node(
    schema: &Value,
    root: bool,
    depth: usize,
    budget: &mut SchemaBudget,
) -> Result<(), SemanticJsonError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(SemanticJsonError::InvalidSchema("schema depth exceeds V1"));
    }
    budget.nodes = budget.nodes.saturating_add(1);
    if budget.nodes > MAX_SCHEMA_NODES {
        return Err(SemanticJsonError::InvalidSchema(
            "schema node count exceeds V1",
        ));
    }
    let object = schema.as_object().ok_or(SemanticJsonError::InvalidSchema(
        "schema nodes must be objects",
    ))?;
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
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(SemanticJsonError::InvalidSchema("unsupported keyword"));
    }
    if !root && object.contains_key("$schema") {
        return Err(SemanticJsonError::InvalidSchema("$schema is root-only"));
    }
    if root && object.get("$schema").and_then(Value::as_str) != Some(SCHEMA_URI) {
        return Err(SemanticJsonError::InvalidSchema(
            "the exact root dialect URI is required",
        ));
    }
    for field in ["title", "description"] {
        if let Some(text) = object.get(field).and_then(Value::as_str) {
            let _ = text;
        } else if object.contains_key(field) {
            return Err(SemanticJsonError::InvalidSchema(
                "title and description must be strings",
            ));
        }
    }
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(SemanticJsonError::InvalidSchema("type is required"))?;
    if root && kind != "object" {
        return Err(SemanticJsonError::InvalidSchema(
            "schema root must be object",
        ));
    }
    if !matches!(
        kind,
        "object" | "array" | "string" | "number" | "integer" | "boolean"
    ) {
        return Err(SemanticJsonError::InvalidSchema("unsupported type"));
    }
    match kind {
        "object" => {
            let properties = object.get("properties").and_then(Value::as_object).ok_or(
                SemanticJsonError::InvalidSchema("object properties are required"),
            )?;
            if properties.len() > MAX_PROPERTIES_PER_OBJECT {
                return Err(SemanticJsonError::InvalidSchema(
                    "too many object properties",
                ));
            }
            budget.properties = budget.properties.saturating_add(properties.len());
            if budget.properties > MAX_PROPERTIES_TOTAL {
                return Err(SemanticJsonError::InvalidSchema(
                    "too many total properties",
                ));
            }
            if object.get("additionalProperties") != Some(&Value::Bool(false)) {
                return Err(SemanticJsonError::InvalidSchema("objects must be closed"));
            }
            let required = object.get("required").and_then(Value::as_array).ok_or(
                SemanticJsonError::InvalidSchema("required must be an array"),
            )?;
            if required.len() != properties.len() {
                return Err(SemanticJsonError::InvalidSchema(
                    "every property must be required exactly once",
                ));
            }
            let mut names = HashSet::new();
            for name in required {
                let name = name.as_str().ok_or(SemanticJsonError::InvalidSchema(
                    "required names must be strings",
                ))?;
                if !properties.contains_key(name) || !names.insert(name) {
                    return Err(SemanticJsonError::InvalidSchema(
                        "required contains an unknown or duplicate property",
                    ));
                }
            }
            for (name, child) in properties {
                let _ = name;
                validate_schema_node(child, false, depth + 1, budget)?;
            }
            reject_keywords(object, &["items", "enum"])?;
        }
        "array" => {
            reject_keywords(
                object,
                &["properties", "required", "additionalProperties", "enum"],
            )?;
            validate_schema_node(
                object
                    .get("items")
                    .ok_or(SemanticJsonError::InvalidSchema("array items are required"))?,
                false,
                depth + 1,
                budget,
            )?;
        }
        _ => reject_keywords(
            object,
            &["properties", "required", "additionalProperties", "items"],
        )?,
    }
    if let Some(values) = object.get("enum") {
        if matches!(kind, "object" | "array") {
            return Err(SemanticJsonError::InvalidSchema("enum must be scalar"));
        }
        let values = values
            .as_array()
            .ok_or(SemanticJsonError::InvalidSchema("enum must be an array"))?;
        if values.is_empty() || values.len() > MAX_ENUM_ENTRIES {
            return Err(SemanticJsonError::InvalidSchema("enum size is outside V1"));
        }
        let mut seen = HashSet::new();
        for value in values {
            if !instance_type_matches(kind, value)
                || matches!(value, Value::Array(_) | Value::Object(_))
            {
                return Err(SemanticJsonError::InvalidSchema(
                    "enum values must match the scalar type",
                ));
            }
            let encoded = canonicalize(value)?;
            if !seen.insert(encoded) {
                return Err(SemanticJsonError::InvalidSchema(
                    "enum contains a duplicate",
                ));
            }
        }
    }
    Ok(())
}

fn reject_keywords(
    object: &Map<String, Value>,
    forbidden: &[&str],
) -> Result<(), SemanticJsonError> {
    if forbidden.iter().any(|key| object.contains_key(*key)) {
        return Err(SemanticJsonError::InvalidSchema(
            "keyword does not apply to type",
        ));
    }
    Ok(())
}

fn validate_instance(schema: &Value, value: &Value) -> Result<(), SemanticJsonError> {
    let object = schema
        .as_object()
        .ok_or(SemanticJsonError::SchemaMismatch)?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(SemanticJsonError::SchemaMismatch)?;
    if !instance_type_matches(kind, value) {
        return Err(SemanticJsonError::SchemaMismatch);
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array)
        && !values
            .iter()
            .any(|candidate| canonicalize(candidate).ok() == canonicalize(value).ok())
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
                validate_instance(
                    properties
                        .get(name)
                        .ok_or(SemanticJsonError::SchemaMismatch)?,
                    child,
                )?;
            }
        }
        ("array", Value::Array(values)) => {
            for child in values {
                validate_instance(
                    object
                        .get("items")
                        .ok_or(SemanticJsonError::SchemaMismatch)?,
                    child,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn instance_type_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.as_f64().is_some_and(f64::is_finite),
        "integer" => value
            .as_f64()
            .is_some_and(|v| v.fract() == 0.0 && v.abs() <= 9_007_199_254_740_991.0),
        "boolean" => value.is_boolean(),
        _ => false,
    }
}

fn validate_schema_strings(value: &Value, total: &mut usize) -> Result<(), SemanticJsonError> {
    match value {
        Value::String(text) => {
            if text.len() > MAX_SCHEMA_STRING_BYTES {
                return Err(SemanticJsonError::InvalidSchema("schema string exceeds V1"));
            }
            *total = total.saturating_add(text.len());
            if *total > MAX_MODEL_VISIBLE_STRING_BYTES {
                return Err(SemanticJsonError::InvalidSchema(
                    "schema string budget exceeded",
                ));
            }
        }
        Value::Array(values) => {
            for child in values {
                validate_schema_strings(child, total)?;
            }
        }
        Value::Object(values) => {
            for (key, child) in values {
                validate_schema_strings(&Value::String(key.clone()), total)?;
                validate_schema_strings(child, total)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_value_bounds(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), SemanticJsonError> {
    *nodes = nodes.saturating_add(1);
    if depth > MAX_DEPTH || *nodes > MAX_NODES {
        return Err(SemanticJsonError::ResourceLimit);
    }
    match value {
        Value::String(text) if text.len() > MAX_STRING_BYTES => {
            return Err(SemanticJsonError::ResourceLimit);
        }
        Value::Number(number) if number.as_f64().is_none_or(|value| !value.is_finite()) => {
            return Err(SemanticJsonError::Numeric);
        }
        Value::Array(values) => {
            for child in values {
                validate_value_bounds(child, depth + 1, nodes)?;
            }
        }
        Value::Object(values) => {
            for (key, child) in values {
                if key.len() > MAX_STRING_BYTES {
                    return Err(SemanticJsonError::ResourceLimit);
                }
                validate_value_bounds(child, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn canonicalize(value: &Value) -> Result<Vec<u8>, SemanticJsonError> {
    canonicalize_prevalidated(value, MAX_BYTES)
}

/// Applies the sole JCS byte codec to a value whose owning type already
/// enforces its own structural/resource bounds.
pub(crate) fn canonicalize_prevalidated(
    value: &Value,
    max_bytes: usize,
) -> Result<Vec<u8>, SemanticJsonError> {
    let out = serde_json_canonicalizer::to_vec(value).map_err(|_| SemanticJsonError::Numeric)?;
    if out.len() > max_bytes {
        return Err(SemanticJsonError::ResourceLimit);
    }
    Ok(out)
}

fn validate_number_lexemes(raw: &str) -> Result<(), SemanticJsonError> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if b == b'-' || b.is_ascii_digit() {
            let start = i;
            if b == b'-' {
                i += 1;
            }
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                i += 1;
                if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                    i += 1;
                }
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let token = &raw[start..i];
            validate_number_lexeme(token)?;
            continue;
        }
        i += 1;
    }
    Ok(())
}

fn validate_number_lexeme(token: &str) -> Result<(), SemanticJsonError> {
    let value = token
        .parse::<f64>()
        .map_err(|_| SemanticJsonError::Numeric)?;
    if !value.is_finite() {
        return Err(SemanticJsonError::Numeric);
    }
    if token.bytes().any(|b| matches!(b, b'1'..=b'9')) && value == 0.0 {
        return Err(SemanticJsonError::Numeric);
    }
    Ok(())
}

/// The [`SemanticJsonV1`] canonical bytes of a value whose owning type
/// bounds its own size: canonical conversation items carry media bytes far
/// past the tool-schema node and byte bounds.
pub(crate) fn canonicalize_normalized_prevalidated(
    value: Value,
    max_bytes: usize,
) -> Result<Vec<u8>, SemanticJsonError> {
    canonicalize_prevalidated(&normalize_numbers(value)?, max_bytes)
}

fn normalize_numbers(value: Value) -> Result<Value, SemanticJsonError> {
    match value {
        Value::Number(number) => portable_json_number(&number).map(Value::Number),
        Value::Array(values) => values
            .into_iter()
            .map(normalize_numbers)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| normalize_numbers(value).map(|value| (key, value)))
            .collect::<Result<Map<_, _>, _>>()
            .map(Value::Object),
        value => Ok(value),
    }
}

/// Keep int53-range whole numbers as JSON integers.
///
/// `Number::from_f64(1.0)` serializes as `1.0`. xAI Responses then rejects
/// JSON Schema keywords such as `minItems` (`got 1.0`). JCS already emits
/// those values without a fraction, so tool digests stay on the integer form.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "int53 range and zero fraction are checked before truncating"
)]
fn portable_json_number(
    number: &serde_json::Number,
) -> Result<serde_json::Number, SemanticJsonError> {
    let value = number
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or(SemanticJsonError::Numeric)?;
    if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER_F64 {
        if value >= 0.0 {
            return Ok(serde_json::Number::from(value as u64));
        }
        return Ok(serde_json::Number::from(value as i64));
    }
    serde_json::Number::from_f64(value).ok_or(SemanticJsonError::Numeric)
}

struct ParseState {
    nodes: usize,
    error: Option<SemanticJsonError>,
}
struct StrictSeed {
    state: Rc<RefCell<ParseState>>,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for StrictSeed {
    type Value = Value;
    fn deserialize<D>(self, d: D) -> Result<Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        {
            let mut s = self.state.borrow_mut();
            s.nodes += 1;
            if s.nodes > MAX_NODES || self.depth > MAX_DEPTH {
                s.error = Some(SemanticJsonError::ResourceLimit);
                return Err(D::Error::custom("limit"));
            }
        }
        d.deserialize_any(StrictVisitor {
            state: self.state,
            depth: self.depth,
        })
    }
}
struct StrictVisitor {
    state: Rc<RefCell<ParseState>>,
    depth: usize,
}
impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("strict JSON")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Value, E> {
        portable_json_number(&serde_json::Number::from(v))
            .map(Value::Number)
            .map_err(E::custom)
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        portable_json_number(&serde_json::Number::from(v))
            .map(Value::Number)
            .map_err(E::custom)
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .ok_or_else(|| E::custom("number"))
            .and_then(|number| portable_json_number(&number).map_err(E::custom))
            .map(Value::Number)
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
        self.visit_string(v.to_owned())
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Value, E> {
        if v.len() > MAX_STRING_BYTES {
            self.state.borrow_mut().error = Some(SemanticJsonError::ResourceLimit);
            return Err(E::custom("limit"));
        }
        Ok(Value::String(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = a.next_element_seed(StrictSeed {
            state: Rc::clone(&self.state),
            depth: self.depth + 1,
        })? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        let mut seen = HashSet::new();
        while let Some(k) = a.next_key::<String>()? {
            if k.len() > MAX_STRING_BYTES {
                self.state.borrow_mut().error = Some(SemanticJsonError::ResourceLimit);
                return Err(A::Error::custom("limit"));
            }
            if !seen.insert(k.clone()) {
                self.state.borrow_mut().error = Some(SemanticJsonError::DuplicateKey);
                return Err(A::Error::custom("duplicate"));
            }
            let v = a.next_value_seed(StrictSeed {
                state: Rc::clone(&self.state),
                depth: self.depth + 1,
            })?;
            out.insert(k, v);
        }
        Ok(Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[expect(
        clippy::needless_pass_by_value,
        reason = "json macro needs owned test values"
    )]
    fn root(properties: Map<String, Value>, required: Vec<Value>) -> Value {
        json!({
            "$schema": SCHEMA_URI,
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        })
    }

    #[test]
    fn strict_parser_rejects_duplicates_trailing_and_numeric_range() {
        assert!(matches!(
            SemanticJsonV1::parse(r#"{"a":1,"a":2}"#),
            Err(SemanticJsonError::DuplicateKey)
        ));
        assert!(matches!(
            SemanticJsonV1::parse(r#"{"a":1,"\u0061":2}"#),
            Err(SemanticJsonError::DuplicateKey)
        ));
        for bad in ["0 1", "1e400", "1e-4000", "NaN", "Infinity", "01"] {
            assert!(SemanticJsonV1::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rfc8785_appendix_b_complete_binary64_vectors() -> Result<(), SemanticJsonError> {
        let vectors = [
            (0x0000_0000_0000_0000, "0"),
            (0x8000_0000_0000_0000, "0"),
            (0x0000_0000_0000_0001, "5e-324"),
            (0x8000_0000_0000_0001, "-5e-324"),
            (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"),
            (0xffef_ffff_ffff_ffff, "-1.7976931348623157e+308"),
            (0x4340_0000_0000_0000, "9007199254740992"),
            (0xc340_0000_0000_0000, "-9007199254740992"),
            (0x4430_0000_0000_0000, "295147905179352830000"),
            (0x44b5_2d02_c7e1_4af5, "9.999999999999997e+22"),
            (0x44b5_2d02_c7e1_4af6, "1e+23"),
            (0x44b5_2d02_c7e1_4af7, "1.0000000000000001e+23"),
            (0x444b_1ae4_d6e2_ef4e, "999999999999999700000"),
            (0x444b_1ae4_d6e2_ef4f, "999999999999999900000"),
            (0x444b_1ae4_d6e2_ef50, "1e+21"),
            (0x3eb0_c6f7_a0b5_ed8c, "9.999999999999997e-7"),
            (0x3eb0_c6f7_a0b5_ed8d, "0.000001"),
            (0x41b3_de43_5555_5553, "333333333.3333332"),
            (0x41b3_de43_5555_5554, "333333333.33333325"),
            (0x41b3_de43_5555_5555, "333333333.3333333"),
            (0x41b3_de43_5555_5556, "333333333.3333334"),
            (0x41b3_de43_5555_5557, "333333333.33333343"),
            (0xbecb_f647_612f_3696, "-0.0000033333333333333333"),
            (0x4314_3ff3_c1cb_0959, "1424953923781206.2"),
        ];
        for (bits, expected) in vectors {
            let value = SemanticJsonV1::from_value(Value::from(f64::from_bits(bits)))?;
            assert_eq!(value.canonical_bytes().as_ref(), expected.as_bytes());
        }
        Ok(())
    }

    #[test]
    fn int53_whole_numbers_serialize_as_json_integers() -> Result<(), SemanticJsonError> {
        let from_int = SemanticJsonV1::from_value(json!({"minItems": 1}))?;
        let from_float = SemanticJsonV1::from_value(json!({"minItems": 1.0}))?;
        assert_eq!(from_int.digest(), from_float.digest());
        let serialized = serde_json::to_string(from_int.value().as_ref())
            .map_err(|_| SemanticJsonError::Invalid)?;
        assert_eq!(serialized, r#"{"minItems":1}"#);
        let serialized_float = serde_json::to_string(from_float.value().as_ref())
            .map_err(|_| SemanticJsonError::Invalid)?;
        assert_eq!(serialized_float, r#"{"minItems":1}"#);
        assert_eq!(from_int.canonical_bytes().as_ref(), b"{\"minItems\":1}");
        Ok(())
    }

    #[test]
    fn rfc8785_property_order_and_reference_corpus() -> Result<(), SemanticJsonError> {
        let vectors = [
            (
                r#"{"\u20ac":"Euro Sign","\r":"Carriage Return","\ufb33":"Hebrew Letter Dalet With Dagesh","1":"One","\ud83d\ude00":"Emoji: Grinning Face","\u0080":"Control","\u00f6":"Latin Small Letter O With Diaeresis"}"#,
                "{\"\\r\":\"Carriage Return\",\"1\":\"One\",\"\u{80}\":\"Control\",\"ö\":\"Latin Small Letter O With Diaeresis\",\"€\":\"Euro Sign\",\"😀\":\"Emoji: Grinning Face\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}",
            ),
            (
                r#"[56,{"d":true,"10":null,"1":[]}]"#,
                r#"[56,{"1":[],"10":null,"d":true}]"#,
            ),
            (
                r#"{"peach":"This sorting order","péché":"is wrong according to French","pêche":"but canonicalization MUST","sin":"ignore locale"}"#,
                r#"{"peach":"This sorting order","péché":"is wrong according to French","pêche":"but canonicalization MUST","sin":"ignore locale"}"#,
            ),
            (
                r#"{"1":{"f":{"f":"hi","F":5},"\n":56.0},"10":{},"":"empty","a":{},"111":[{"e":"yes","E":"no"}],"A":{}}"#,
                "{\"\":\"empty\",\"1\":{\"\\n\":56,\"f\":{\"F\":5,\"f\":\"hi\"}},\"10\":{},\"111\":[{\"E\":\"no\",\"e\":\"yes\"}],\"A\":{},\"a\":{}}",
            ),
            (
                r#"{"Unnormalized Unicode":"A\u030a"}"#,
                "{\"Unnormalized Unicode\":\"A\u{030a}\"}",
            ),
            (
                r#"{"numbers":[333333333.33333329,1E30,4.50,2e-3,0.000000000000000000000000001],"string":"\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/","literals":[null,true,false]}"#,
                "{\"literals\":[null,true,false],\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\"string\":\"€$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}",
            ),
            (
                r#"{"\u20ac":"Euro Sign","\r":"Carriage Return","\u000a":"Newline","1":"One","\u0080":"Control\u007f","\ud83d\ude02":"Smiley","\u00f6":"Latin Small Letter O With Diaeresis","\ufb33":"Hebrew Letter Dalet With Dagesh","</script>":"Browser Challenge"}"#,
                "{\"\\n\":\"Newline\",\"\\r\":\"Carriage Return\",\"1\":\"One\",\"</script>\":\"Browser Challenge\",\"\u{80}\":\"Control\u{7f}\",\"ö\":\"Latin Small Letter O With Diaeresis\",\"€\":\"Euro Sign\",\"😂\":\"Smiley\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}",
            ),
        ];
        for (raw, expected) in vectors {
            assert_eq!(
                SemanticJsonV1::parse(raw)?.canonical_bytes().as_ref(),
                expected.as_bytes()
            );
        }
        Ok(())
    }

    #[test]
    fn jcs_uses_raw_utf16_property_order_and_preserves_arrays() -> Result<(), SemanticJsonError> {
        let v = SemanticJsonV1::parse(r#"{"\uE000":1,"\uD834\uDD1E":2,"a":[2,1]}"#)?;
        assert_eq!(
            v.canonical_bytes().as_ref(),
            "{\"a\":[2,1],\"𝄞\":2,\"\":1}".as_bytes()
        );
        Ok(())
    }

    #[test]
    fn parser_enforces_depth_node_string_and_byte_bounds() {
        let too_deep = format!(
            "{}0{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        );
        assert!(matches!(
            SemanticJsonV1::parse(&too_deep),
            Err(SemanticJsonError::ResourceLimit)
        ));
        let too_many_nodes = format!("[{}]", "0,".repeat(MAX_NODES));
        assert!(matches!(
            SemanticJsonV1::parse(&too_many_nodes),
            Err(SemanticJsonError::ResourceLimit)
        ));
        let too_long = format!("\"{}\"", "x".repeat(MAX_STRING_BYTES + 1));
        assert!(matches!(
            SemanticJsonV1::parse(&too_long),
            Err(SemanticJsonError::ResourceLimit)
        ));
        let too_many_bytes = format!(" \"{}\"", "x".repeat(MAX_BYTES));
        assert!(matches!(
            SemanticJsonV1::parse(&too_many_bytes),
            Err(SemanticJsonError::ResourceLimit)
        ));
    }

    #[test]
    fn parser_accepts_exact_instance_bounds_and_rejects_plus_one() {
        let at_depth = format!("{}0{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(SemanticJsonV1::parse(&at_depth).is_ok());
        let at_nodes = format!(
            "[{}]",
            std::iter::repeat_n("0", MAX_NODES - 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(SemanticJsonV1::parse(&at_nodes).is_ok());
        let at_string = format!("\"{}\"", "x".repeat(MAX_STRING_BYTES));
        assert!(SemanticJsonV1::parse(&at_string).is_ok());
        let at_bytes = format!("0{}", " ".repeat(MAX_BYTES - 1));
        assert!(SemanticJsonV1::parse(&at_bytes).is_ok());
        assert!(matches!(
            SemanticJsonV1::parse(&(at_bytes + " ")),
            Err(SemanticJsonError::ResourceLimit)
        ));
    }

    #[test]
    fn parser_rejects_adversarial_number_grammar_and_surrogates() {
        for bad in [
            "+1",
            ".1",
            "1.",
            "1e",
            "1e+",
            "--1",
            "00",
            "-01",
            "[1,]",
            r#""\ud800""#,
        ] {
            assert!(SemanticJsonV1::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn schema_requires_exact_closed_required_object_shape() {
        let good = root(
            Map::from_iter([("x".into(), json!({"type":"string"}))]),
            vec![json!("x")],
        );
        assert!(PortableToolSchemaV1::new(good).is_ok());
        for bad in [
            json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            json!({"$schema":"http://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}),
            json!({"$schema":SCHEMA_URI,"type":"string"}),
            root(
                Map::from_iter([("x".into(), json!({"type":"string"}))]),
                vec![],
            ),
            root(
                Map::from_iter([("x".into(), json!({"type":"string"}))]),
                vec![json!("x"), json!("x")],
            ),
            root(
                Map::from_iter([("x".into(), json!({"type":"string"}))]),
                vec![json!("y")],
            ),
            root(
                Map::from_iter([(
                    "x".into(),
                    json!({"type":"object","properties":{},"required":[],"additionalProperties":true}),
                )]),
                vec![json!("x")],
            ),
        ] {
            assert!(PortableToolSchemaV1::new(bad).is_err());
        }
    }

    #[test]
    fn schema_accepts_only_v1_types_keywords_and_scalar_enums() {
        for kind in ["string", "number", "integer", "boolean"] {
            let schema = root(
                Map::from_iter([("x".into(), json!({"type":kind}))]),
                vec![json!("x")],
            );
            assert!(PortableToolSchemaV1::new(schema).is_ok(), "{kind}");
        }
        let array = root(
            Map::from_iter([(
                "x".into(),
                json!({"type":"array","items":{"type":"boolean"}}),
            )]),
            vec![json!("x")],
        );
        assert!(PortableToolSchemaV1::new(array).is_ok());
        for child in [
            json!({"type":"null"}),
            json!({"type":["string","null"]}),
            json!({"type":"string","minLength":1}),
            json!({"type":"number","minimum":0}),
            json!({"type":"array","items":true}),
            json!({"type":"array"}),
            json!({"type":"string","properties":{}}),
            json!({"type":"string","enum":[]}),
            json!({"type":"string","enum":["a","a"]}),
            json!({"type":"integer","enum":[1.5]}),
            json!({"type":"object","properties":{},"required":[],"additionalProperties":false,"enum":[true]}),
        ] {
            let schema = root(Map::from_iter([("x".into(), child)]), vec![json!("x")]);
            assert!(PortableToolSchemaV1::new(schema).is_err());
        }
    }

    #[test]
    fn schema_enforces_property_enum_depth_and_string_bounds() {
        let properties = (0..=MAX_PROPERTIES_PER_OBJECT)
            .map(|index| (format!("p{index}"), json!({"type":"boolean"})))
            .collect::<Map<_, _>>();
        let required = properties.keys().cloned().map(Value::String).collect();
        assert!(PortableToolSchemaV1::new(root(properties, required)).is_err());

        let enums = (0..=MAX_ENUM_ENTRIES)
            .map(|index| json!(index))
            .collect::<Vec<_>>();
        let schema = root(
            Map::from_iter([("x".into(), json!({"type":"number","enum":enums}))]),
            vec![json!("x")],
        );
        assert!(PortableToolSchemaV1::new(schema).is_err());

        let schema = root(
            Map::from_iter([(
                "x".into(),
                json!({"type":"string","description":"x".repeat(MAX_SCHEMA_STRING_BYTES + 1)}),
            )]),
            vec![json!("x")],
        );
        assert!(PortableToolSchemaV1::new(schema).is_err());

        let mut child = json!({"type":"boolean"});
        for _ in 0..=MAX_SCHEMA_DEPTH {
            child = json!({"type":"array","items":child});
        }
        let schema = root(Map::from_iter([("x".into(), child)]), vec![json!("x")]);
        assert!(PortableToolSchemaV1::new(schema).is_err());
    }

    #[test]
    fn schema_accepts_independent_exact_v1_boundaries() -> Result<(), SemanticJsonError> {
        let properties = (0..MAX_PROPERTIES_PER_OBJECT)
            .map(|index| (format!("p{index}"), json!({"type":"boolean"})))
            .collect::<Map<_, _>>();
        let required = properties.keys().cloned().map(Value::String).collect();
        PortableToolSchemaV1::new(root(properties, required))?;

        let values = (0..MAX_ENUM_ENTRIES).map(Value::from).collect::<Vec<_>>();
        PortableToolSchemaV1::new(root(
            Map::from_iter([("x".into(), json!({"type":"integer","enum":values}))]),
            vec![json!("x")],
        ))?;

        let mut child = json!({"type":"boolean"});
        for _ in 0..MAX_SCHEMA_DEPTH - 1 {
            child = json!({"type":"array","items":child});
        }
        PortableToolSchemaV1::new(root(
            Map::from_iter([("x".into(), child)]),
            vec![json!("x")],
        ))?;

        PortableToolSchemaV1::new(root(
            Map::from_iter([(
                "x".into(),
                json!({"type":"string","description":"x".repeat(MAX_SCHEMA_STRING_BYTES)}),
            )]),
            vec![json!("x")],
        ))?;

        let mut visible = root(
            (0..8)
                .map(|index| {
                    (
                        format!("p{index}"),
                        json!({"type":"string","description":""}),
                    )
                })
                .collect(),
            (0..8).map(|index| json!(format!("p{index}"))).collect(),
        );
        let mut baseline = 0;
        validate_schema_strings(&visible, &mut baseline)?;
        let mut remaining = MAX_MODEL_VISIBLE_STRING_BYTES - baseline;
        let properties = visible["properties"]
            .as_object_mut()
            .ok_or(SemanticJsonError::InvalidSchema("test properties"))?;
        for index in 0..8 {
            let take = remaining.min(MAX_SCHEMA_STRING_BYTES);
            properties[&format!("p{index}")]["description"] = json!("x".repeat(take));
            remaining -= take;
        }
        assert_eq!(remaining, 0);
        let mut total = 0;
        validate_schema_strings(&visible, &mut total)?;
        assert_eq!(total, MAX_MODEL_VISIBLE_STRING_BYTES);
        PortableToolSchemaV1::new(visible)?;
        Ok(())
    }

    #[test]
    fn maximum_legal_schema_and_adversarial_plus_one_validate() -> Result<(), SemanticJsonError> {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for group in 0..4 {
            let mut nested = Map::new();
            let mut nested_required = Vec::new();
            for leaf in 0..63 {
                let name = format!("p{group}_{leaf}");
                nested.insert(name.clone(), json!({"type":"integer"}));
                nested_required.push(Value::String(name));
            }
            let name = format!("g{group}");
            properties.insert(
                name.clone(),
                json!({
                    "type":"object", "properties":nested,
                    "required":nested_required, "additionalProperties":false
                }),
            );
            required.push(Value::String(name));
        }
        let maximum = PortableToolSchemaV1::new(root(properties, required))?;
        let mut instance = Map::new();
        for group in 0..4 {
            let nested = (0..63)
                .map(|leaf| (format!("p{group}_{leaf}"), Value::from(leaf)))
                .collect();
            instance.insert(format!("g{group}"), Value::Object(nested));
        }
        maximum.validate(&SemanticJsonV1::from_value(Value::Object(instance))?)?;

        let mut too_many = Map::new();
        let mut too_many_required = Vec::new();
        for index in 0..=MAX_PROPERTIES_PER_OBJECT {
            let name = format!("p{index}");
            too_many.insert(name.clone(), json!({"type":"boolean"}));
            too_many_required.push(Value::String(name));
        }
        assert!(PortableToolSchemaV1::new(root(too_many, too_many_required)).is_err());
        Ok(())
    }

    #[test]
    fn instance_validation_enforces_required_types_enum_and_safe_integer()
    -> Result<(), SemanticJsonError> {
        let schema = PortableToolSchemaV1::new(root(
            Map::from_iter([
                ("choice".into(), json!({"type":"string","enum":["a","b"]})),
                ("count".into(), json!({"type":"integer"})),
                (
                    "flags".into(),
                    json!({"type":"array","items":{"type":"boolean"}}),
                ),
            ]),
            vec![json!("choice"), json!("count"), json!("flags")],
        ))?;
        assert!(
            schema
                .parse_instance(r#"{"choice":"a","count":9007199254740991,"flags":[true]}"#)
                .is_ok()
        );
        for bad in [
            r#"{"choice":"a","count":1}"#,
            r#"{"choice":"c","count":1,"flags":[]}"#,
            r#"{"choice":"a","count":1.5,"flags":[]}"#,
            r#"{"choice":"a","count":9007199254740992,"flags":[]}"#,
            r#"{"choice":"a","count":1,"flags":[0]}"#,
            r#"{"choice":"a","count":1,"flags":[],"extra":true}"#,
        ] {
            assert!(matches!(
                schema.parse_instance(bad),
                Err(SemanticJsonError::SchemaMismatch)
            ));
        }
        Ok(())
    }

    #[test]
    fn serde_reconstruction_verifies_revision_canonical_bytes_and_digest()
    -> Result<(), Box<dyn std::error::Error>> {
        let value = SemanticJsonV1::parse(r#"{"a":1}"#)?;
        let encoded = serde_json::to_string(&value)?;
        assert_eq!(serde_json::from_str::<SemanticJsonV1>(&encoded)?, value);
        let mut wire = serde_json::to_value(&value)?;
        wire["revision"] = json!(SEMANTIC_JSON_REVISION + 1);
        assert!(serde_json::from_value::<SemanticJsonV1>(wire).is_err());

        for field in ["canonical", "digest"] {
            let mut wire = serde_json::to_value(&value)?;
            wire[field] = json!([0]);
            assert!(
                serde_json::from_value::<SemanticJsonV1>(wire).is_err(),
                "{field}"
            );
        }
        let mut wire = serde_json::to_value(&value)?;
        wire["unknown"] = json!(true);
        assert!(serde_json::from_value::<SemanticJsonV1>(wire).is_err());
        Ok(())
    }
}
