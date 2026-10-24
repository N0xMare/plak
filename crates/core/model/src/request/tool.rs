use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{
    Digest, DigestDomain, PortableToolSchemaV1, PortableToolSchemaV2, ToolName, ToolResourceKey,
};

use super::{RequestError, ensure_nonempty};

/// Bounded UTF-8 input or output contract, distinct from JSON schema.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct Utf8TextContractV1 {
    max_bytes: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Utf8TextContractV1Wire {
    max_bytes: u32,
}

impl<'de> Deserialize<'de> for Utf8TextContractV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = Utf8TextContractV1Wire::deserialize(deserializer)?;
        Self::new(wire.max_bytes).map_err(serde::de::Error::custom)
    }
}

impl Utf8TextContractV1 {
    /// Creates a nonzero bounded UTF-8 contract.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::TooLong`] when the bound is zero or exceeds the
    /// format's fixed one-mebibyte ceiling.
    pub fn new(max_bytes: u32) -> Result<Self, RequestError> {
        if max_bytes == 0 || max_bytes > 1_048_576 {
            return Err(RequestError::TooLong {
                field: "UTF-8 contract",
                maximum: 1_048_576,
            });
        }
        Ok(Self { max_bytes })
    }
    /// Returns the exact byte limit.
    #[must_use]
    pub const fn max_bytes(self) -> u32 {
        self.max_bytes
    }
    /// Validates one text value.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::TooLong`] when the UTF-8 byte length exceeds
    /// the declared bound.
    pub fn validate(self, text: &str) -> Result<(), RequestError> {
        if text.len() > self.max_bytes as usize {
            return Err(RequestError::TooLong {
                field: "UTF-8 value",
                maximum: self.max_bytes as usize,
            });
        }
        Ok(())
    }
}

/// Explicit local tool input meaning. Schema shape never selects a wire form.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolInputContract {
    /// Strict semantic JSON validated by `PortableToolSchemaV2`.
    Structured(PortableToolSchemaV2),
    /// Raw bounded UTF-8 text for an explicitly free-form tool.
    Utf8(Utf8TextContractV1),
}

impl ToolInputContract {
    /// Builds a structured contract through the sole V2 schema authority.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema is outside the portable V2 subset.
    pub fn structured(schema: Value) -> Result<Self, RequestError> {
        Ok(Self::Structured(PortableToolSchemaV2::new(schema)?))
    }
    /// Builds an explicit free-form UTF-8 contract.
    ///
    /// # Errors
    ///
    /// Returns an error when `max_bytes` is zero or exceeds the fixed bound.
    pub fn utf8(max_bytes: u32) -> Result<Self, RequestError> {
        Ok(Self::Utf8(Utf8TextContractV1::new(max_bytes)?))
    }
}

/// Bounded ordered model-content output contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ModelContentContractV1 {
    max_parts: u32,
    max_bytes: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelContentContractV1Wire {
    max_parts: u32,
    max_bytes: u32,
}

impl<'de> Deserialize<'de> for ModelContentContractV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelContentContractV1Wire::deserialize(deserializer)?;
        Self::new(wire.max_parts, wire.max_bytes).map_err(serde::de::Error::custom)
    }
}

impl ModelContentContractV1 {
    /// Fixed V1 ceiling for ordered model-content parts.
    pub const MAX_PARTS: u32 = 128;
    /// Fixed V1 ceiling for encoded model-content bytes.
    pub const MAX_BYTES: u32 = 1_048_576;

    /// Creates a nonzero bounded model-content contract.
    ///
    /// # Errors
    ///
    /// Returns an error when either bound is zero or exceeds its fixed V1 ceiling.
    pub fn new(max_parts: u32, max_bytes: u32) -> Result<Self, RequestError> {
        if max_parts == 0 || max_parts > Self::MAX_PARTS {
            return Err(RequestError::TooLong {
                field: "model-content part contract",
                maximum: Self::MAX_PARTS as usize,
            });
        }
        if max_bytes == 0 || max_bytes > Self::MAX_BYTES {
            return Err(RequestError::TooLong {
                field: "model-content byte contract",
                maximum: Self::MAX_BYTES as usize,
            });
        }
        Ok(Self {
            max_parts,
            max_bytes,
        })
    }

    /// Returns the exact ordered-part limit.
    #[must_use]
    pub const fn max_parts(self) -> u32 {
        self.max_parts
    }

    /// Returns the exact encoded-byte limit.
    #[must_use]
    pub const fn max_bytes(self) -> u32 {
        self.max_bytes
    }
}

/// Explicit local tool output meaning.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolOutputContract {
    /// Strict semantic JSON validated by `PortableToolSchemaV1`.
    Structured(PortableToolSchemaV1),
    /// Exactly one bounded UTF-8 text result.
    Utf8(Utf8TextContractV1),
    /// Bounded ordered model-content parts.
    ModelContent(ModelContentContractV1),
}

impl ToolOutputContract {
    /// Builds a structured output contract through the sole V1 schema authority.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema is outside the portable V1 subset.
    pub fn structured(schema: Value) -> Result<Self, RequestError> {
        Ok(Self::Structured(PortableToolSchemaV1::new(schema)?))
    }
    /// Builds an explicit UTF-8 output contract.
    ///
    /// # Errors
    ///
    /// Returns an error when `max_bytes` is zero or exceeds the fixed bound.
    pub fn utf8(max_bytes: u32) -> Result<Self, RequestError> {
        Ok(Self::Utf8(Utf8TextContractV1::new(max_bytes)?))
    }
    /// Builds an explicit ordered model-content output contract.
    ///
    /// # Errors
    ///
    /// Returns an error when either bound is zero or exceeds its fixed V1 ceiling.
    pub fn model_content(max_parts: u32, max_bytes: u32) -> Result<Self, RequestError> {
        Ok(Self::ModelContent(ModelContentContractV1::new(
            max_parts, max_bytes,
        )?))
    }

    /// Builds the explicit portable-maximum model-content contract.
    #[must_use]
    pub const fn portable_model_content() -> Self {
        Self::ModelContent(ModelContentContractV1 {
            max_parts: ModelContentContractV1::MAX_PARTS,
            max_bytes: ModelContentContractV1::MAX_BYTES,
        })
    }
}

/// The scheduler concurrency class declared by a tool definition.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolConcurrency {
    /// Calls must be serialized with other effectful work.
    Serial,
    /// Independent calls may execute concurrently.
    Parallel,
    /// Calls exclude every other call naming any of the same resource keys.
    ResourceExclusive(Vec<ToolResourceKey>),
}

impl ToolConcurrency {
    const MAX_RESOURCE_KEYS: usize = 16;

    fn normalize(&mut self) -> Result<(), RequestError> {
        let Self::ResourceExclusive(keys) = self else {
            return Ok(());
        };
        if keys.is_empty() {
            return Err(RequestError::InvalidToolConcurrency {
                reason: "resource-exclusive concurrency requires at least one key",
            });
        }
        if keys.len() > Self::MAX_RESOURCE_KEYS {
            return Err(RequestError::InvalidToolConcurrency {
                reason: "resource-exclusive concurrency exceeds 16 keys",
            });
        }
        keys.sort();
        if keys.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(RequestError::InvalidToolConcurrency {
                reason: "resource-exclusive concurrency contains a duplicate key",
            });
        }
        Ok(())
    }
}

/// The side-effect class declared by a tool definition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolEffect {
    /// The tool is declared read-only.
    ReadOnly,
    /// The tool may mutate state or affect external systems.
    Mutating,
}

/// An immutable canonical model-facing tool definition.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolDefinition {
    name: ToolName,
    semantic_version: String,
    description: String,
    input: ToolInputContract,
    output: ToolOutputContract,
    concurrency: ToolConcurrency,
    effect: ToolEffect,
    digest: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolDefinitionWire {
    name: ToolName,
    semantic_version: String,
    description: String,
    input: ToolInputContract,
    output: ToolOutputContract,
    concurrency: ToolConcurrency,
    effect: ToolEffect,
    digest: Digest,
}

impl<'de> Deserialize<'de> for ToolDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolDefinitionWire::deserialize(deserializer)?;
        let definition = Self::new(
            wire.name,
            wire.semantic_version,
            wire.description,
            wire.input,
            wire.output,
            wire.concurrency,
            wire.effect,
        )
        .map_err(serde::de::Error::custom)?;
        if definition.digest != wire.digest {
            return Err(serde::de::Error::custom(RequestError::IntegrityMismatch {
                field: "tool definition",
            }));
        }
        Ok(definition)
    }
}

impl ToolDefinition {
    /// Creates and integrity-digests a validated tool definition.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or oversized metadata, unsupported schema
    /// structure, canonicalization failure, or digest construction failure.
    pub fn new(
        name: ToolName,
        semantic_version: impl Into<String>,
        description: impl Into<String>,
        input: ToolInputContract,
        output: ToolOutputContract,
        mut concurrency: ToolConcurrency,
        effect: ToolEffect,
    ) -> Result<Self, RequestError> {
        let semantic_version = semantic_version.into();
        let description = description.into();
        ensure_nonempty(&semantic_version, "tool semantic version")?;
        ensure_nonempty(&description, "tool description")?;
        if semantic_version.len() > 128 {
            return Err(RequestError::TooLong {
                field: "tool semantic version",
                maximum: 128,
            });
        }
        if description.len() > 4_096 {
            return Err(RequestError::TooLong {
                field: "tool description",
                maximum: 4_096,
            });
        }
        let input_identity = tool_input_identity(&input);
        let output_identity = tool_output_identity(&output);
        concurrency.normalize()?;
        let concurrency_text = match &concurrency {
            ToolConcurrency::Serial => b"serial".to_vec(),
            ToolConcurrency::Parallel => b"parallel".to_vec(),
            ToolConcurrency::ResourceExclusive(keys) => {
                let mut encoded = b"resource-exclusive".to_vec();
                for key in keys {
                    encoded.push(0);
                    encoded.extend_from_slice(key.as_str().as_bytes());
                }
                encoded
            }
        };
        let effect_text = match effect {
            ToolEffect::ReadOnly => b"read-only".as_slice(),
            ToolEffect::Mutating => b"mutating".as_slice(),
        };
        let digest = Digest::sha256(
            DigestDomain::ToolDefinition,
            4,
            &[
                name.as_str().as_bytes(),
                semantic_version.as_bytes(),
                description.as_bytes(),
                &input_identity,
                &output_identity,
                &concurrency_text,
                effect_text,
            ],
        )?;
        Ok(Self {
            name,
            semantic_version,
            description,
            input,
            output,
            concurrency,
            effect,
            digest,
        })
    }

    /// Returns the model-facing name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Returns the semantic version.
    #[must_use]
    pub fn semantic_version(&self) -> &str {
        &self.semantic_version
    }

    /// Returns the model-facing description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the exact input contract.
    #[must_use]
    pub const fn input(&self) -> &ToolInputContract {
        &self.input
    }

    /// Returns the exact output contract.
    #[must_use]
    pub const fn output(&self) -> &ToolOutputContract {
        &self.output
    }

    /// Returns the declared concurrency class.
    #[must_use]
    pub fn concurrency(&self) -> ToolConcurrency {
        self.concurrency.clone()
    }

    /// Returns the declared effect class.
    #[must_use]
    pub const fn effect(&self) -> ToolEffect {
        self.effect
    }

    /// Returns the canonical definition digest.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

fn tool_input_identity(contract: &ToolInputContract) -> Vec<u8> {
    match contract {
        ToolInputContract::Structured(schema) => {
            [b"structured:".as_slice(), schema.digest().bytes()].concat()
        }
        ToolInputContract::Utf8(limit) => {
            [b"utf8:".as_slice(), &limit.max_bytes().to_be_bytes()].concat()
        }
    }
}

fn tool_output_identity(contract: &ToolOutputContract) -> Vec<u8> {
    match contract {
        ToolOutputContract::Structured(schema) => {
            [b"structured:".as_slice(), schema.digest().bytes()].concat()
        }
        ToolOutputContract::Utf8(limit) => {
            [b"utf8:".as_slice(), &limit.max_bytes().to_be_bytes()].concat()
        }
        ToolOutputContract::ModelContent(limit) => [
            b"model-content:".as_slice(),
            &limit.max_parts().to_be_bytes(),
            &limit.max_bytes().to_be_bytes(),
        ]
        .concat(),
    }
}
