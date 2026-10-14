//! Local validation for model-owned semantic tool contracts.

use std::fmt;
use std::sync::Arc;

use plakous_model::{Digest, DigestDomain, SemanticJsonV1, ToolDefinition, ToolInputContract};
pub use plakous_model::{PORTABLE_TOOL_SCHEMA_V2_REVISION, PortableToolSchemaV2};
use serde_json::Value;

use crate::{ToolRuntimeError, ToolRuntimeErrorKind};

const SEMANTIC_JSON_V1_MAX_BYTES: usize = 1_048_576;
const SEMANTIC_JSON_V1_MAX_DEPTH: usize = 32;
const SEMANTIC_JSON_V1_MAX_NODES: usize = 32_768;
const SEMANTIC_JSON_V1_MAX_STRING_BYTES: usize = 262_144;

/// Registration limits layered below the model-owned fixed V1 schema bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaLimits {
    /// Maximum canonical schema bytes accepted by this registry.
    pub max_bytes: usize,
}

impl Default for SchemaLimits {
    fn default() -> Self {
        Self { max_bytes: 65_536 }
    }
}

/// Hard local limits applied before input reaches an executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArgumentLimits {
    /// Maximum exact input bytes.
    pub max_bytes: usize,
    /// Maximum nested structured-input depth (never wider than `SemanticJsonV1`).
    pub max_depth: usize,
    /// Maximum structured-input nodes (never wider than `SemanticJsonV1`).
    pub max_nodes: usize,
    /// Maximum bytes in one string or key (never wider than `SemanticJsonV1`).
    pub max_string_bytes: usize,
}

impl Default for ArgumentLimits {
    fn default() -> Self {
        Self {
            max_bytes: SEMANTIC_JSON_V1_MAX_BYTES,
            max_depth: SEMANTIC_JSON_V1_MAX_DEPTH,
            max_nodes: SEMANTIC_JSON_V1_MAX_NODES,
            max_string_bytes: SEMANTIC_JSON_V1_MAX_STRING_BYTES,
        }
    }
}

/// Strictly parsed and contract-validated invocation input.
#[derive(Clone)]
pub struct ValidatedArguments {
    value: Arc<Value>,
    canonical: Arc<[u8]>,
    text: Option<Arc<str>>,
    digest: Digest,
    raw_bytes: usize,
}

impl ValidatedArguments {
    /// Returns the structured value, or a string value for explicit UTF-8 input.
    #[must_use]
    pub const fn value(&self) -> &Arc<Value> {
        &self.value
    }
    /// Returns semantic JCS bytes for structured input or exact UTF-8 bytes.
    #[must_use]
    pub const fn canonical(&self) -> &Arc<[u8]> {
        &self.canonical
    }
    /// Returns explicit free-form UTF-8 input only for a UTF-8 contract.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
    /// Returns the exact contract-bound input digest.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
    /// Returns exact received byte count.
    #[must_use]
    pub const fn raw_bytes(&self) -> usize {
        self.raw_bytes
    }
}

impl fmt::Debug for ValidatedArguments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidatedArguments")
            .field("raw_bytes", &self.raw_bytes)
            .field("canonical_bytes", &self.canonical.len())
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

/// Frozen local validator for one already valid model-owned input contract.
#[derive(Clone, Debug)]
pub struct ValidatedToolSchema {
    contract: ToolInputContract,
    schema_digest: Digest,
    limits: ArgumentLimits,
}

impl ValidatedToolSchema {
    /// Freezes an already completely validated definition without repair.
    ///
    /// # Errors
    ///
    /// Returns an error when the definition uses an unsupported contract,
    /// exceeds local limits, or cannot be assigned an exact identity.
    pub fn compile(
        definition: &ToolDefinition,
        schema_limits: SchemaLimits,
        limits: ArgumentLimits,
    ) -> Result<Self, ToolRuntimeError> {
        validate_argument_limits(limits)?;
        let schema_digest = match definition.input() {
            ToolInputContract::Structured(schema) => {
                if schema.document().canonical_bytes().len() > schema_limits.max_bytes {
                    return Err(resource("tool schema exceeds registry byte limit"));
                }
                schema.digest()
            }
            ToolInputContract::Utf8(contract) => Digest::sha256(
                DigestDomain::ToolDefinition,
                1,
                &[
                    b"plakous-tools/utf8-input-v1",
                    &contract.max_bytes().to_be_bytes(),
                ],
            )
            .map_err(|_| invalid("UTF-8 input identity failed"))?,
            _ => return Err(invalid("unsupported input contract revision")),
        };
        Ok(Self {
            contract: definition.input().clone(),
            schema_digest,
            limits,
        })
    }

    /// Returns the exact input-contract digest.
    #[must_use]
    pub const fn schema_digest(&self) -> Digest {
        self.schema_digest
    }

    /// Validates one exact provider/tool-call payload before policy or execution.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload exceeds local limits or does not
    /// satisfy the registered input contract exactly.
    pub fn validate_arguments(&self, raw: &str) -> Result<ValidatedArguments, ToolRuntimeError> {
        if raw.len() > self.limits.max_bytes {
            return Err(resource("tool input exceeds its byte limit"));
        }
        let (value, canonical, text) = match &self.contract {
            ToolInputContract::Structured(schema) => {
                let semantic = SemanticJsonV1::parse(raw)
                    .map_err(|_| arguments("tool input is not valid structured semantic JSON"))?;
                validate_structured_limits(semantic.value(), self.limits)?;
                schema.validate(&semantic).map_err(|_| {
                    arguments(
                        numeric_bound_hint(schema.document().value(), semantic.value())
                            .unwrap_or_else(|| {
                                "tool input does not satisfy its structured contract".to_owned()
                            }),
                    )
                })?;
                (
                    Arc::clone(semantic.value()),
                    Arc::clone(semantic.canonical_bytes()),
                    None,
                )
            }
            ToolInputContract::Utf8(contract) => {
                contract
                    .validate(raw)
                    .map_err(|_| resource("tool UTF-8 input exceeds its contract"))?;
                (
                    Arc::new(Value::String(raw.to_owned())),
                    Arc::from(raw.as_bytes()),
                    Some(Arc::from(raw)),
                )
            }
            _ => return Err(arguments("unsupported input contract revision")),
        };
        let digest = Digest::sha256(
            DigestDomain::EffectAuthorization,
            2,
            &[
                b"plakous-tools/validated-input-v2",
                self.schema_digest.bytes(),
                &canonical,
            ],
        )
        .map_err(|_| arguments("tool input identity failed"))?;
        Ok(ValidatedArguments {
            value,
            canonical,
            text,
            digest,
            raw_bytes: raw.len(),
        })
    }
}

pub(crate) fn validate_argument_limits(limits: ArgumentLimits) -> Result<(), ToolRuntimeError> {
    if limits.max_bytes == 0
        || limits.max_depth == 0
        || limits.max_nodes == 0
        || limits.max_string_bytes == 0
    {
        return Err(invalid("tool input limits must be nonzero"));
    }
    if limits.max_bytes > SEMANTIC_JSON_V1_MAX_BYTES
        || limits.max_depth > SEMANTIC_JSON_V1_MAX_DEPTH
        || limits.max_nodes > SEMANTIC_JSON_V1_MAX_NODES
        || limits.max_string_bytes > SEMANTIC_JSON_V1_MAX_STRING_BYTES
    {
        return Err(invalid(
            "tool input limits exceed SemanticJsonV1 fixed limits",
        ));
    }
    Ok(())
}

fn validate_structured_limits(
    value: &Value,
    limits: ArgumentLimits,
) -> Result<(), ToolRuntimeError> {
    fn walk(
        value: &Value,
        depth: usize,
        nodes: &mut usize,
        limits: ArgumentLimits,
    ) -> Result<(), ToolRuntimeError> {
        *nodes = nodes.saturating_add(1);
        if depth > limits.max_depth || *nodes > limits.max_nodes {
            return Err(resource("tool input exceeds its structural limits"));
        }
        match value {
            Value::String(text) if text.len() > limits.max_string_bytes => {
                return Err(resource("tool input exceeds its string limit"));
            }
            Value::Array(values) => {
                for child in values {
                    walk(child, depth.saturating_add(1), nodes, limits)?;
                }
            }
            Value::Object(values) => {
                for (key, child) in values {
                    if key.len() > limits.max_string_bytes {
                        return Err(resource("tool input exceeds its string limit"));
                    }
                    walk(child, depth.saturating_add(1), nodes, limits)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    walk(value, 0, &mut 0, limits)
}

fn invalid(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidDefinition, message)
}
// Validation remains owned by the model schema. This diagnostic only names
// a declared numeric constraint, never the submitted value or sibling input.
fn numeric_bound_hint(schema: &Value, input: &Value) -> Option<String> {
    for (name, property) in schema.get("properties")?.as_object()? {
        let Some(value) = input.get(name).and_then(Value::as_f64) else {
            continue;
        };
        if name.len() > 64
            || !name
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
        {
            continue;
        }
        for (keyword, relation) in [("minimum", "at least"), ("maximum", "at most")] {
            let Some(bound) = property.get(keyword) else {
                continue;
            };
            let Some(limit) = bound.as_f64() else {
                continue;
            };
            if (keyword == "minimum" && value < limit) || (keyword == "maximum" && value > limit) {
                return Some(format!(
                    "tool input rejected: {name} must be {relation} {bound}; no tool was dispatched"
                ));
            }
        }
    }
    None
}

fn arguments(message: impl AsRef<str>) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidArguments, message)
}
fn resource(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::ResourceLimit, message)
}

/// One closed JSON Schema object with exactly the named required properties.
///
/// Every tool schema in the workspace is this shape: an object, a closed
/// property set, an explicit required list, and no additional properties. It
/// lives here rather than with any one pack because the packs, the
/// compositions and the marker definitions all write it.
#[allow(
    clippy::needless_pass_by_value,
    reason = "callers construct owned JSON objects that become part of the returned schema"
)]
#[must_use]
pub fn object_schema(properties: Value, required: &[&str]) -> Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_model::{
        PortableToolSchemaV2, ToolConcurrency, ToolEffect, ToolInputContract, ToolName,
        ToolOutputContract,
    };
    use serde_json::json;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn definition() -> TestResult<ToolDefinition> {
        definition_with_schema(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{"value":{"type":"integer"}},
            "required":["value"],
            "additionalProperties":false
        }))
    }

    fn definition_with_schema(schema: Value) -> TestResult<ToolDefinition> {
        Ok(ToolDefinition::new(
            ToolName::new("lookup")?,
            "1",
            "lookup",
            ToolInputContract::Structured(PortableToolSchemaV2::new(schema)?),
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )?)
    }

    #[test]
    fn numeric_rejection_names_the_limit_without_echoing_other_arguments() -> TestResult {
        let definition = definition_with_schema(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object", "properties":{
                "timeout":{"type":"integer","minimum":0,"maximum":300_000},
                "command":{"type":"string"}
            }, "required":["command"], "additionalProperties":false
        }))?;
        let schema = ValidatedToolSchema::compile(
            &definition,
            SchemaLimits::default(),
            ArgumentLimits::default(),
        )?;
        let error = schema
            .validate_arguments(r#"{"command":"private-command-sentinel","timeout":600000}"#)
            .err()
            .ok_or("an excessive timeout must fail validation")?;
        assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidArguments);
        assert!(
            error
                .message()
                .as_str()
                .contains("timeout must be at most 300000")
        );
        assert!(
            !error
                .message()
                .as_str()
                .contains("private-command-sentinel")
        );
        assert!(
            schema
                .validate_arguments(r#"{"command":"ok","timeout":300000}"#)
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn local_validation_accepts_an_exact_portable_instance() -> TestResult {
        let schema = ValidatedToolSchema::compile(
            &definition()?,
            SchemaLimits::default(),
            ArgumentLimits::default(),
        )?;
        assert!(schema.validate_arguments(r#"{"value":2}"#).is_ok());
        assert!(
            schema
                .validate_arguments(r#"{"value":2,"value":3}"#)
                .is_err()
        );
        assert!(
            schema
                .validate_arguments(r#"{"value":2,"optional":null}"#)
                .is_err()
        );
        assert!(
            schema
                .validate_arguments(r#"{"value":2,"optional":"x"}"#)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn compile_rejects_zero_or_wider_argument_limits() -> TestResult {
        let defaults = ArgumentLimits::default();
        let invalid_limits = [
            ArgumentLimits {
                max_bytes: 0,
                ..defaults
            },
            ArgumentLimits {
                max_depth: 0,
                ..defaults
            },
            ArgumentLimits {
                max_nodes: 0,
                ..defaults
            },
            ArgumentLimits {
                max_string_bytes: 0,
                ..defaults
            },
            ArgumentLimits {
                max_bytes: defaults.max_bytes + 1,
                ..defaults
            },
            ArgumentLimits {
                max_depth: defaults.max_depth + 1,
                ..defaults
            },
            ArgumentLimits {
                max_nodes: defaults.max_nodes + 1,
                ..defaults
            },
            ArgumentLimits {
                max_string_bytes: defaults.max_string_bytes + 1,
                ..defaults
            },
        ];
        for limits in invalid_limits {
            let error =
                ValidatedToolSchema::compile(&definition()?, SchemaLimits::default(), limits)
                    .err()
                    .ok_or("invalid argument limits compiled")?;
            assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidDefinition);
        }
        Ok(())
    }

    #[test]
    fn structured_byte_limit_accepts_exactly_at_limit_and_rejects_plus_one() -> TestResult {
        let schema = ValidatedToolSchema::compile(
            &definition_with_schema(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{},
                "required":[],
                "additionalProperties":false
            }))?,
            SchemaLimits::default(),
            ArgumentLimits {
                max_bytes: 2,
                ..ArgumentLimits::default()
            },
        )?;
        assert!(schema.validate_arguments("{}").is_ok());
        assert_eq!(
            schema
                .validate_arguments(" {}")
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::ResourceLimit)
        );
        Ok(())
    }

    #[test]
    fn structured_depth_limit_accepts_exactly_at_limit_and_rejects_plus_one() -> TestResult {
        let limits = ArgumentLimits {
            max_depth: 2,
            ..ArgumentLimits::default()
        };
        let at_limit = ValidatedToolSchema::compile(
            &definition_with_schema(array_definition(1))?,
            SchemaLimits::default(),
            limits,
        )?;
        let plus_one = ValidatedToolSchema::compile(
            &definition_with_schema(array_definition(2))?,
            SchemaLimits::default(),
            limits,
        )?;
        assert!(at_limit.validate_arguments(r#"{"value":[1]}"#).is_ok());
        assert_eq!(
            plus_one
                .validate_arguments(r#"{"value":[[1]]}"#)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::ResourceLimit)
        );
        Ok(())
    }

    #[test]
    fn structured_node_limit_accepts_exactly_at_limit_and_rejects_plus_one() -> TestResult {
        let schema = ValidatedToolSchema::compile(
            &definition_with_schema(array_definition(1))?,
            SchemaLimits::default(),
            ArgumentLimits {
                max_nodes: 4,
                ..ArgumentLimits::default()
            },
        )?;
        assert!(schema.validate_arguments(r#"{"value":[1,2]}"#).is_ok());
        assert_eq!(
            schema
                .validate_arguments(r#"{"value":[1,2,3]}"#)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::ResourceLimit)
        );
        Ok(())
    }

    #[test]
    fn structured_string_limit_accepts_exactly_at_limit_and_rejects_plus_one() -> TestResult {
        let limits = ArgumentLimits {
            max_string_bytes: 4,
            ..ArgumentLimits::default()
        };
        let value_schema = ValidatedToolSchema::compile(
            &definition_with_schema(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"v":{"type":"string"}},
                "required":["v"],
                "additionalProperties":false
            }))?,
            SchemaLimits::default(),
            limits,
        )?;
        assert!(value_schema.validate_arguments(r#"{"v":"1234"}"#).is_ok());
        assert_eq!(
            value_schema
                .validate_arguments(r#"{"v":"12345"}"#)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::ResourceLimit)
        );

        let at_limit_key = ValidatedToolSchema::compile(
            &definition_with_schema(key_definition("1234"))?,
            SchemaLimits::default(),
            limits,
        )?;
        let plus_one_key = ValidatedToolSchema::compile(
            &definition_with_schema(key_definition("12345"))?,
            SchemaLimits::default(),
            limits,
        )?;
        assert!(at_limit_key.validate_arguments(r#"{"1234":1}"#).is_ok());
        assert_eq!(
            plus_one_key
                .validate_arguments(r#"{"12345":1}"#)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::ResourceLimit)
        );
        Ok(())
    }

    fn array_definition(nested_arrays: usize) -> Value {
        let mut item = json!({"type":"integer"});
        for _ in 0..nested_arrays {
            item = json!({"type":"array", "items":item});
        }
        json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{"value":item},
            "required":["value"],
            "additionalProperties":false
        })
    }

    fn key_definition(key: &str) -> Value {
        json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{key:{"type":"integer"}},
            "required":[key],
            "additionalProperties":false
        })
    }
}
