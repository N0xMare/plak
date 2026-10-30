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
        Self::from_value(strict_parse(raw, MAX_BYTES, SCHEMA_LIMITS)?)
    }

    /// Strictly parses one JSON document whose owner bounds it by bytes
    /// alone: the same duplicate-key, number and UTF-8 rules as
    /// [`Self::parse`], without the tool-schema node and string limits, which
    /// model output such as a tool call writing a large file legitimately
    /// exceeds.
    ///
    /// # Errors
    ///
    /// Returns a content-free error for malformed, non-I-JSON, or over-bound input.
    pub fn parse_document(raw: &str, max_bytes: usize) -> Result<Value, SemanticJsonError> {
        normalize_numbers(strict_parse(
            raw,
            max_bytes,
            ParseLimits {
                depth: MAX_DOCUMENT_DEPTH,
                nodes: usize::MAX,
                string_bytes: max_bytes,
            },
        )?)
    }

    /// The canonical bytes [`Self::from_value`] would produce for a document
    /// whose owner bounds it by bytes alone (a request prefix carrying a
    /// whole tool catalog, a conversation item carrying an image). Valid
    /// [`SemanticJsonV1`] input yields identical bytes.
    ///
    /// # Errors
    ///
    /// Returns a content-free error for non-I-JSON numbers or more than
    /// `max_bytes` canonical bytes.
    pub fn canonical_document_bytes(
        value: Value,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SemanticJsonError> {
        canonicalize_normalized_prevalidated(value, max_bytes)
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

fn strict_parse(
    raw: &str,
    max_bytes: usize,
    limits: ParseLimits,
) -> Result<Value, SemanticJsonError> {
    if raw.len() > max_bytes {
        return Err(SemanticJsonError::ResourceLimit);
    }
    validate_number_lexemes(raw)?;
    let state = Rc::new(RefCell::new(ParseState {
        nodes: 0,
        limits,
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
    Ok(value)
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

/// Rewrites every number in `value` the way canonical identities spell it, so
/// an owned value and its identity never disagree on a number's spelling.
pub(crate) fn with_canonical_numbers(value: Value) -> Value {
    match value {
        // A serde_json number is finite, so the portable form always exists.
        Value::Number(number) => Value::Number(portable_json_number(&number).unwrap_or(number)),
        Value::Array(values) => {
            Value::Array(values.into_iter().map(with_canonical_numbers).collect())
        }
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, with_canonical_numbers(value)))
                .collect(),
        ),
        value => value,
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
    limits: ParseLimits,
    error: Option<SemanticJsonError>,
}

/// Structural limits of one strict parse.
#[derive(Clone, Copy)]
struct ParseLimits {
    depth: usize,
    nodes: usize,
    string_bytes: usize,
}

/// The tool-schema limits [`SemanticJsonV1`] enforces.
const SCHEMA_LIMITS: ParseLimits = ParseLimits {
    depth: MAX_DEPTH,
    nodes: MAX_NODES,
    string_bytes: MAX_STRING_BYTES,
};

/// Below `serde_json`'s own recursion limit.
const MAX_DOCUMENT_DEPTH: usize = 126;
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
            if s.nodes > s.limits.nodes || self.depth > s.limits.depth {
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
        let limit = self.state.borrow().limits.string_bytes;
        if v.len() > limit {
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
        let limit = self.state.borrow().limits.string_bytes;
        while let Some(k) = a.next_key::<String>()? {
            if k.len() > limit {
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
mod tests;
