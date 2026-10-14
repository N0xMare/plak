//! Bounded authenticated OpenRouter catalog listing and exact Chat discovery.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::fmt;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use plakous_http::{
    Client, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request,
    SensitiveBytes, StatusCode, sensitive_header_value,
};
use plakous_model::{Clock as _, Digest, DigestDomain, ModelId, SystemClock, Timestamp};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    OpenRouterChatBuildError, OpenRouterChatEndpointCandidate, OpenRouterChatModelEvidence,
    OpenRouterChatRoute, OpenRouterReasoningFacts, OpenRouterToolChoiceModes,
};

const MODELS_PATH: &str = "/api/v1/models";
const ENDPOINTS_PREFIX: &str = "/api/v1/models";
const MAX_MODELS: usize = 4096;
const MAX_ENDPOINTS: usize = 256;
const MAX_PARAMETERS: usize = 256;
const MAX_PARAMETER_BYTES: usize = 128;
const MAX_REASONING_EFFORTS: usize = 16;
const MAX_REASONING_EFFORT_BYTES: usize = 64;

/// Explicit bounds for one two-request authenticated discovery operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenRouterChatDiscoveryConfig {
    /// Complete two-request operation timeout.
    pub operation_timeout: Duration,
    /// Maximum retained bytes for each JSON response.
    pub maximum_body_bytes: usize,
    /// Freshness interval assigned to the exact result.
    pub evidence_ttl: Duration,
}

impl Default for OpenRouterChatDiscoveryConfig {
    fn default() -> Self {
        Self {
            operation_timeout: Duration::from_secs(30),
            maximum_body_bytes: 1024 * 1024,
            evidence_ttl: Duration::from_secs(24 * 60 * 60),
        }
    }
}

impl OpenRouterChatRoute {
    /// Lists catalog model identifiers with one bounded authenticated GET.
    /// Catalog presence does not establish capabilities or admission evidence.
    pub async fn list_models(
        &self,
        client: &Client,
        config: OpenRouterChatDiscoveryConfig,
    ) -> Result<Vec<String>, OpenRouterChatDiscoveryError> {
        validate_config(config)?;
        let operation = Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[
                b"plakous-openrouter/catalog-list-v1",
                self.identity().fingerprint().as_bytes(),
                MODELS_PATH.as_bytes(),
            ],
        )?;
        let generation = self.current_credential_generation_grant()?;
        let key = self.catalog_credential(&operation, generation)?;
        let deadline = Instant::now()
            .checked_add(config.operation_timeout)
            .ok_or(OpenRouterChatDiscoveryError::InvalidPolicy)?;
        let body = get_json(
            client,
            Origin::parse(self.identity().origin().as_str())?,
            MODELS_PATH,
            key.bearer_bytes(),
            deadline,
            config.maximum_body_bytes,
        )
        .await?;
        catalog_model_ids(&parse_unique_json(&body)?)
    }

    /// Authenticates and discovers one exact literal Chat model plus endpoints.
    ///
    /// This operation performs exactly two GET requests with one resolved key
    /// generation. It never dispatches inference and never promotes catalog
    /// parameters into capabilities or qualification.
    pub async fn discover_chat_model(
        &self,
        client: &Client,
        literal: &str,
        config: OpenRouterChatDiscoveryConfig,
    ) -> Result<OpenRouterChatModelEvidence, OpenRouterChatDiscoveryError> {
        validate_config(config)?;
        let (author, slug, model) = exact_literal(literal)?;
        let endpoint_path = format!(
            "{ENDPOINTS_PREFIX}/{}/{}/endpoints",
            encode_path_component(author),
            encode_path_component(slug)
        );
        let operation = Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[
                b"plakous-openrouter/chat-discovery-v1",
                self.identity().fingerprint().as_bytes(),
                literal.as_bytes(),
                MODELS_PATH.as_bytes(),
                endpoint_path.as_bytes(),
            ],
        )?;
        let generation = self.current_credential_generation_grant()?;
        let key = self.catalog_credential(&operation, generation)?;
        let deadline = Instant::now()
            .checked_add(config.operation_timeout)
            .ok_or(OpenRouterChatDiscoveryError::InvalidPolicy)?;
        let origin = Origin::parse(self.identity().origin().as_str())?;
        let models = get_json(
            client,
            origin.clone(),
            MODELS_PATH,
            key.bearer_bytes(),
            deadline,
            config.maximum_body_bytes,
        )
        .await?;
        let endpoints = get_json(
            client,
            origin,
            &endpoint_path,
            key.bearer_bytes(),
            deadline,
            config.maximum_body_bytes,
        )
        .await?;
        let model_value = parse_unique_json(&models)?;
        let endpoints_value = parse_unique_json(&endpoints)?;
        let model_facts = validate_models_document(&model_value, literal)?;
        let candidates = validate_endpoints_document(&endpoints_value, literal)?;
        let observed_at = SystemClock.now();
        let expires_at = Timestamp::new(
            observed_at
                .seconds()
                .checked_add(config.evidence_ttl.as_secs())
                .ok_or(OpenRouterChatDiscoveryError::InvalidPolicy)?,
            observed_at.nanoseconds(),
        )?;
        OpenRouterChatModelEvidence::from_authenticated_discovery(
            model,
            self.identity(),
            candidates,
            model_facts.supported_parameters,
            model_facts.reasoning,
            model_facts.context_length,
            observed_at,
            expires_at,
        )
        .map_err(OpenRouterChatDiscoveryError::from)
    }
}

fn validate_config(
    config: OpenRouterChatDiscoveryConfig,
) -> Result<(), OpenRouterChatDiscoveryError> {
    if config.operation_timeout.is_zero()
        || config.maximum_body_bytes == 0
        || config.maximum_body_bytes > 4 * 1024 * 1024
        || config.evidence_ttl.is_zero()
        || config.evidence_ttl.as_secs() > crate::OPENROUTER_EVIDENCE_MAX_AGE_SECONDS
        || config.evidence_ttl.subsec_nanos() != 0
    {
        return Err(OpenRouterChatDiscoveryError::InvalidPolicy);
    }
    Ok(())
}

fn exact_literal(literal: &str) -> Result<(&str, &str, ModelId), OpenRouterChatDiscoveryError> {
    let Some((author, slug)) = literal.split_once('/') else {
        return Err(OpenRouterChatDiscoveryError::InvalidLiteral);
    };
    if author.is_empty()
        || slug.is_empty()
        || slug.contains('/')
        || !safe_author(author)
        || !safe_slug(slug)
    {
        return Err(OpenRouterChatDiscoveryError::InvalidLiteral);
    }
    let model = ModelId::new(literal).map_err(|_| OpenRouterChatDiscoveryError::InvalidLiteral)?;
    Ok((author, slug, model))
}

fn safe_author(value: &str) -> bool {
    value.len() <= 128
        && !matches!(value, "." | "..")
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn safe_slug(value: &str) -> bool {
    value.len() <= 192
        && !matches!(value, "." | "..")
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':')
        })
}

fn encode_path_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str("%3A");
        }
    }
    encoded
}

async fn get_json(
    client: &Client,
    origin: Origin,
    path: &str,
    bearer: Vec<u8>,
    deadline: Instant,
    maximum_body_bytes: usize,
) -> Result<Vec<u8>, OpenRouterChatDiscoveryError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("accept"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("authorization"),
        sensitive_header_value(bearer).map_err(|_| {
            OpenRouterChatDiscoveryError::Authentication(
                crate::OpenRouterAuthError::CredentialSource,
            )
        })?,
    );
    let request = Request::new(
        origin,
        Method::GET,
        path,
        headers,
        SensitiveBytes::new(Vec::new()).into_bytes(),
        client.limits(),
    )?;
    let response = client
        .execute(request, ExchangeControl::new(deadline))
        .await?;
    if response.status() != StatusCode::OK {
        let _ = response.into_body().close().await;
        return Err(OpenRouterChatDiscoveryError::UnexpectedStatus);
    }
    if !response
        .headers()
        .get(&HeaderName::from_static("content-type"))
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        let _ = response.into_body().close().await;
        return Err(OpenRouterChatDiscoveryError::UnexpectedMediaType);
    }
    let mut body = response.into_body();
    let mut retained = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk?;
        if retained
            .len()
            .checked_add(chunk.len())
            .is_none_or(|size| size > maximum_body_bytes)
        {
            let _ = body.close().await;
            return Err(OpenRouterChatDiscoveryError::ResourceLimit);
        }
        retained.extend_from_slice(&chunk);
    }
    if retained.is_empty() {
        return Err(OpenRouterChatDiscoveryError::EmptyBody);
    }
    Ok(retained)
}

fn parse_unique_json(bytes: &[u8]) -> Result<Value, OpenRouterChatDiscoveryError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = UniqueJson::deserialize(&mut deserializer)
        .map_err(|_| OpenRouterChatDiscoveryError::InvalidJson)?
        .0;
    deserializer
        .end()
        .map_err(|_| OpenRouterChatDiscoveryError::InvalidJson)?;
    Ok(value)
}

struct UniqueJson(Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> Visitor<'de> for UniqueJsonVisitor {
    type Value = UniqueJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded JSON without duplicate object members")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueJson(Value::Bool(value)))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueJson(Value::Number(value.into())))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueJson(Value::Number(value.into())))
    }
    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueJson)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueJson(Value::String(value.to_owned())))
    }
    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueJson(Value::String(value)))
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJson(Value::Null))
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJson(Value::Null))
    }
    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        UniqueJson::deserialize(deserializer)
    }
    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(value) = sequence.next_element::<UniqueJson>()? {
            values.push(value.0);
        }
        Ok(UniqueJson(Value::Array(values)))
    }
    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate JSON object member"));
            }
            let value = map.next_value::<UniqueJson>()?;
            values.insert(key, value.0);
        }
        Ok(UniqueJson(Value::Object(values)))
    }
}

struct ModelFacts {
    supported_parameters: BTreeSet<String>,
    reasoning: Option<OpenRouterReasoningFacts>,
    context_length: u64,
}

pub(crate) fn catalog_snapshot_identity(
    route_fingerprint: &str,
    literal: &str,
    context_length: u64,
    parameters: &BTreeSet<String>,
    reasoning: Option<&OpenRouterReasoningFacts>,
    candidates: &[OpenRouterChatEndpointCandidate],
) -> Result<Digest, OpenRouterChatDiscoveryError> {
    let mut candidates: Vec<_> = candidates.iter().collect();
    candidates.sort_by_key(|candidate| &candidate.endpoint);
    let snapshot =
        serde_json::to_vec(&(literal, context_length, parameters, reasoning, &candidates))
            .map_err(|_| {
                OpenRouterChatDiscoveryError::InvalidCatalogValue("catalog.identity".into())
            })?;
    Ok(Digest::sha256(
        DigestDomain::ProviderSemanticEvidence,
        1,
        &[
            b"plakous-openrouter/catalog-snapshot-v1",
            route_fingerprint.as_bytes(),
            &snapshot,
        ],
    )?)
}

#[cfg(test)]
fn snapshot_from_facts(
    route_fingerprint: &str,
    literal: &str,
    facts: &ModelFacts,
    candidates: &[OpenRouterChatEndpointCandidate],
) -> Result<Digest, OpenRouterChatDiscoveryError> {
    catalog_snapshot_identity(
        route_fingerprint,
        literal,
        facts.context_length,
        &facts.supported_parameters,
        facts.reasoning.as_ref(),
        candidates,
    )
}

const OUTPUT_DEFAULT: u32 = 4_096;
const REASONING_OUTPUT_DEFAULT: u32 = 16_384;
const OUTPUT_CEILING: u32 = 32_768;
const OUTPUT_FLOOR: u32 = 4_096;
const OUTPUT_RUNGS: [u32; 4] = [4_096, 8_192, 16_384, 32_768];
const INPUT_CEILING: u64 = 65_536;

#[derive(Clone, Copy, Debug)]
pub(crate) enum AdmissionWire {
    Chat,
    Responses,
    Messages,
}

impl AdmissionWire {
    fn from_protocol(protocol: &str) -> Result<Self, OpenRouterChatBuildError> {
        match protocol {
            crate::OPENROUTER_CHAT_PROTOCOL_ID => Ok(Self::Chat),
            crate::OPENROUTER_RESPONSES_PROTOCOL_ID => Ok(Self::Responses),
            crate::OPENROUTER_MESSAGES_PROTOCOL_ID => Ok(Self::Messages),
            _ => Err(OpenRouterChatBuildError::InvalidDiscoveryEvidence),
        }
    }

    fn profile(self) -> (&'static str, &'static str, u32) {
        match self {
            Self::Chat => (
                crate::OPENROUTER_CHAT_PROTOCOL_ID,
                crate::chat::OPENROUTER_CHAT_PROFILE,
                crate::OPENROUTER_CHAT_DIALECT_REVISION,
            ),
            Self::Responses => (
                crate::OPENROUTER_RESPONSES_PROTOCOL_ID,
                crate::OPENROUTER_RESPONSES_PROFILE,
                crate::OPENROUTER_RESPONSES_DIALECT_REVISION,
            ),
            Self::Messages => (
                crate::OPENROUTER_MESSAGES_PROTOCOL_ID,
                crate::messages::PROFILE,
                crate::OPENROUTER_MESSAGES_DIALECT_REVISION,
            ),
        }
    }

    fn adapter_revision(self) -> &'static str {
        match self {
            Self::Chat => crate::chat::OPENROUTER_CHAT_LOWERING_REVISION,
            Self::Responses => crate::execution::OPENROUTER_ADAPTER_REVISION,
            Self::Messages => crate::messages::LOWERING_REVISION,
        }
    }

    fn uses_parameter(self, parameter: &str) -> bool {
        matches!(parameter, "tools" | "max_tokens" | "reasoning")
            || (parameter == "stop" && !matches!(self, Self::Responses))
            || (parameter == "tool_choice" && !matches!(self, Self::Chat))
    }

    fn eligible(
        self,
        endpoint: &OpenRouterChatEndpointCandidate,
        reasoning: bool,
        tools: bool,
    ) -> bool {
        let parameters = &endpoint.supported_parameters;
        (!tools || parameters.contains("tools"))
            && parameters.contains("max_tokens")
            && (matches!(self, Self::Chat) || parameters.contains("tool_choice"))
            && (!matches!(self, Self::Messages) || endpoint.tool_choice.auto)
            && (!reasoning || parameters.contains("reasoning"))
    }
}

#[derive(Serialize)]
struct AdmissionEnvelope {
    available: bool,
    minimum_prompt: Option<u64>,
    minimum_completion: Option<u64>,
    minimum_context: Option<u64>,
    endpoints: BTreeSet<EndpointAdmissionFacts>,
    output_default: u32,
    output_ceiling: u32,
    output_half_window_cap: u64,
    output_floor: u32,
    output_rungs: [u32; 4],
    output_bound: u64,
    input_ceiling: u64,
    maximum_projected_input: Option<u64>,
}

#[derive(Eq, PartialEq, Ord, PartialOrd, Serialize)]
struct EndpointAdmissionFacts {
    parameters: BTreeSet<String>,
    tool_choice: [bool; 4],
    implicit_caching: bool,
    reasoning: bool,
}

impl OpenRouterChatModelEvidence {
    fn admission_envelope(
        &self,
        wire: AdmissionWire,
        reasoning: bool,
        tools: bool,
    ) -> AdmissionEnvelope {
        let endpoints: Vec<_> = self
            .endpoints()
            .iter()
            .filter(|endpoint| wire.eligible(endpoint, reasoning, tools))
            .collect();
        let output_default = if reasoning {
            REASONING_OUTPUT_DEFAULT
        } else {
            OUTPUT_DEFAULT
        };
        let attested = endpoints
            .iter()
            .filter_map(|endpoint| endpoint.max_completion_tokens)
            .min();
        let budget = attested
            .unwrap_or(u64::from(output_default))
            .min(u64::from(OUTPUT_CEILING))
            .min(self.context_length() / 2);
        let output_bound = OUTPUT_RUNGS
            .iter()
            .rev()
            .find(|rung| u64::from(**rung) <= budget)
            .map_or(budget, |rung| u64::from(*rung))
            .max(u64::from(OUTPUT_FLOOR));
        let safe_input = self.projected_input(&endpoints, output_bound).ok();
        AdmissionEnvelope {
            available: !endpoints.is_empty(),
            minimum_prompt: endpoints
                .iter()
                .map(|endpoint| endpoint.max_prompt_tokens)
                .min()
                .flatten(),
            minimum_completion: endpoints
                .iter()
                .map(|endpoint| endpoint.max_completion_tokens)
                .min()
                .flatten(),
            minimum_context: endpoints
                .iter()
                .map(|endpoint| endpoint.context_length)
                .min()
                .flatten(),
            endpoints: endpoints
                .iter()
                .map(|endpoint| {
                    let choice = endpoint.tool_choice;
                    let explicit_choice = !matches!(wire, AdmissionWire::Chat);
                    EndpointAdmissionFacts {
                        parameters: endpoint
                            .supported_parameters
                            .iter()
                            .filter(|parameter| wire.uses_parameter(parameter))
                            .cloned()
                            .collect(),
                        tool_choice: [
                            explicit_choice && choice.none,
                            explicit_choice && choice.auto,
                            explicit_choice && choice.required,
                            explicit_choice && choice.function,
                        ],
                        implicit_caching: endpoint.supports_implicit_caching,
                        reasoning: endpoint.supported_parameters.contains("reasoning"),
                    }
                })
                .collect(),
            output_default,
            output_ceiling: OUTPUT_CEILING,
            output_half_window_cap: self.context_length() / 2,
            output_floor: OUTPUT_FLOOR,
            output_rungs: OUTPUT_RUNGS,
            output_bound,
            input_ceiling: INPUT_CEILING,
            maximum_projected_input: safe_input,
        }
    }

    fn projected_input(
        &self,
        endpoints: &[&OpenRouterChatEndpointCandidate],
        output: u64,
    ) -> Result<u64, OpenRouterChatBuildError> {
        if endpoints.is_empty() {
            return Err(OpenRouterChatBuildError::UnboundedAdmission);
        }
        let mut safe_input = self
            .context_length()
            .saturating_sub(output)
            .min(INPUT_CEILING);
        for endpoint in endpoints {
            if endpoint
                .max_completion_tokens
                .is_none_or(|bound| bound < output)
            {
                return Err(OpenRouterChatBuildError::UnprovenCompletionBound);
            }
            let prompt = endpoint
                .max_prompt_tokens
                .or_else(|| {
                    endpoint
                        .context_length
                        .and_then(|context| context.checked_sub(output))
                })
                .filter(|bound| *bound > 0)
                .ok_or(OpenRouterChatBuildError::UnprovenPromptBound)?;
            safe_input = safe_input.min(prompt);
        }
        (safe_input > 0)
            .then_some(safe_input)
            .ok_or(OpenRouterChatBuildError::UnboundedAdmission)
    }

    pub(crate) fn admission_identity(
        &self,
        wire: AdmissionWire,
    ) -> Result<Digest, OpenRouterChatBuildError> {
        // Bind both reasoning dispositions: the selected configuration binds
        // which one is used, and neither may acquire a weaker gateway envelope.
        let facts = serde_json::to_vec(&(
            self.model(),
            wire.profile(),
            wire.adapter_revision(),
            self.context_length(),
            self.reasoning(),
            self.supported_parameters()
                .iter()
                .filter(|parameter| wire.uses_parameter(parameter))
                .collect::<Vec<_>>(),
            self.admission_envelope(wire, false, true),
            self.admission_envelope(wire, true, true),
            matches!(wire, AdmissionWire::Chat).then(|| {
                (
                    self.admission_envelope(wire, false, false),
                    self.admission_envelope(wire, true, false),
                )
            }),
        ))
        .map_err(|_| OpenRouterChatBuildError::Identity)?;
        Ok(Digest::sha256(
            DigestDomain::ProviderSemanticEvidence,
            1,
            &[b"plakous-openrouter/wire-admission-v2", &facts],
        )?)
    }

    /// Checks availability of an endpoint for the complete agent tool request.
    pub fn gateway_admits(
        &self,
        protocol: &str,
        reasoning_on_wire: bool,
    ) -> Result<bool, OpenRouterChatBuildError> {
        let wire = AdmissionWire::from_protocol(protocol)?;
        Ok(self
            .endpoints()
            .iter()
            .any(|endpoint| wire.eligible(endpoint, reasoning_on_wire, true)))
    }

    /// Derives the bounded gateway output reservation for one exact wire.
    pub fn gateway_output_bound(
        &self,
        protocol: &str,
        reasoning_on_wire: bool,
    ) -> Result<NonZeroU32, OpenRouterChatBuildError> {
        let wire = AdmissionWire::from_protocol(protocol)?;
        u32::try_from(
            self.admission_envelope(
                wire,
                reasoning_on_wire,
                !matches!(wire, AdmissionWire::Chat),
            )
            .output_bound,
        )
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or(OpenRouterChatBuildError::InvalidDiscoveryEvidence)
    }

    /// Proves the input ceiling over every endpoint eligible for the exact wire,
    /// including Chat tool-omitting rounds, compaction and final answers.
    pub fn gateway_maximum_projected_input(
        &self,
        protocol: &str,
        reasoning_on_wire: bool,
        output_bound: NonZeroU32,
    ) -> Result<u64, OpenRouterChatBuildError> {
        let wire = AdmissionWire::from_protocol(protocol)?;
        if !self.gateway_admits(protocol, reasoning_on_wire)? {
            return Err(OpenRouterChatBuildError::UnboundedAdmission);
        }
        let endpoints: Vec<_> = self
            .endpoints()
            .iter()
            .filter(|endpoint| {
                wire.eligible(
                    endpoint,
                    reasoning_on_wire,
                    !matches!(wire, AdmissionWire::Chat),
                )
            })
            .collect();
        self.projected_input(&endpoints, u64::from(output_bound.get()))
    }
}

fn catalog_model_ids(value: &Value) -> Result<Vec<String>, OpenRouterChatDiscoveryError> {
    let root = catalog_object(value, "models", &["data"])?;
    let rows = root["data"]
        .as_array()
        .ok_or_else(|| catalog_value("models.data"))?;
    if rows.len() > MAX_MODELS {
        return Err(OpenRouterChatDiscoveryError::ResourceLimit);
    }
    let mut seen = BTreeSet::new();
    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let literal = bounded_string(row.as_object().and_then(|row| row.get("id")))?;
        if !seen.insert(literal) {
            return Err(catalog_value("models.data[].id"));
        }
        models.push(literal.to_owned());
    }
    Ok(models)
}

fn validate_models_document(
    value: &Value,
    literal: &str,
) -> Result<ModelFacts, OpenRouterChatDiscoveryError> {
    let root = catalog_object(value, "models", &["data"])?;
    let rows = root["data"]
        .as_array()
        .ok_or_else(|| catalog_value("models.data"))?;
    if rows.is_empty() || rows.len() > MAX_MODELS {
        return Err(OpenRouterChatDiscoveryError::ResourceLimit);
    }
    let mut matched = None;
    for row in rows {
        let id = bounded_string(row.as_object().and_then(|object| object.get("id")))?;
        if id != literal {
            continue;
        }
        let row = catalog_object(
            row,
            "models.data[]",
            &[
                "id",
                "canonical_slug",
                "context_length",
                "supported_parameters",
            ],
        )?;
        let canonical = bounded_string(row.get("canonical_slug"))?;
        // The literal `id` is the requested gateway identity. OpenRouter may
        // map variants such as `:free` to a dated canonical slug, so the
        // canonical field is validated as bounded evidence but never used as
        // an equality or routing authority.
        if !safe_endpoint(canonical) || matched.is_some() {
            return Err(OpenRouterChatDiscoveryError::AmbiguousModel);
        }
        let context_length = row["context_length"]
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(|| catalog_value("models.data[].context_length"))?;
        let supported_parameters = bounded_parameters(&row["supported_parameters"])?;
        let reasoning = parse_reasoning_facts(row.get("reasoning"))?;
        matched = Some(ModelFacts {
            supported_parameters,
            reasoning,
            context_length,
        });
    }
    matched.ok_or(OpenRouterChatDiscoveryError::AmbiguousModel)
}

fn validate_endpoints_document(
    value: &Value,
    literal: &str,
) -> Result<Vec<OpenRouterChatEndpointCandidate>, OpenRouterChatDiscoveryError> {
    let root = catalog_object(value, "endpoints", &["data"])?;
    let data = catalog_object(
        root.get("data")
            .ok_or_else(|| catalog_value("endpoints.data"))?,
        "endpoints.data",
        &["endpoints", "id"],
    )?;
    if bounded_string(data.get("id"))? != literal {
        return Err(OpenRouterChatDiscoveryError::AmbiguousModel);
    }
    let endpoints = data["endpoints"]
        .as_array()
        .ok_or_else(|| catalog_value("endpoints.data.endpoints"))?;
    if endpoints.is_empty() || endpoints.len() > MAX_ENDPOINTS {
        return Err(OpenRouterChatDiscoveryError::ResourceLimit);
    }
    let mut result = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    for endpoint in endpoints {
        let endpoint = catalog_object(
            endpoint,
            "endpoints.data.endpoints[]",
            &["tag", "provider_name", "supported_parameters"],
        )?;
        let tag = bounded_string(endpoint.get("tag"))?;
        let provider_name = bounded_string(endpoint.get("provider_name"))?;
        if !safe_endpoint(tag) {
            return Err(catalog_value("endpoints.data.endpoints[].tag"));
        }
        let supported_parameters = bounded_parameters(&endpoint["supported_parameters"])?;
        let context_length = optional_positive_u64(endpoint.get("context_length"))?;
        let max_prompt_tokens = optional_positive_u64(endpoint.get("max_prompt_tokens"))?;
        let max_completion_tokens = optional_positive_u64(endpoint.get("max_completion_tokens"))?;
        let tool_choice = parse_tool_choice_modes(
            endpoint.get("supports_tool_choice"),
            "endpoints.data.endpoints[].supports_tool_choice",
        )?;
        let supports_implicit_caching = optional_bool(
            endpoint.get("supports_implicit_caching"),
            "endpoints.data.endpoints[].supports_implicit_caching",
        )?;
        let candidate = OpenRouterChatEndpointCandidate {
            endpoint: tag.to_owned(),
            provider_name: provider_name.to_owned(),
            supported_parameters,
            context_length,
            max_prompt_tokens,
            max_completion_tokens,
            tool_choice,
            supports_implicit_caching,
        };
        // Telemetry can differ between repeated rows for one routing tag.
        // Identical admission facts coalesce. Disagreement makes the tag
        // ambiguous: it stays out of the candidate set, and discovery fails
        // only when that leaves no candidate.
        if !ambiguous.contains(tag) {
            match result.entry(tag.to_owned()) {
                Entry::Vacant(entry) => {
                    entry.insert(candidate);
                }
                Entry::Occupied(entry) if entry.get() == &candidate => {}
                Entry::Occupied(entry) => {
                    drop(entry.remove());
                    ambiguous.insert(tag.to_owned());
                }
            }
        }
    }
    if result.is_empty() {
        return Err(catalog_value("endpoints.data.endpoints[].tag"));
    }
    Ok(result.into_values().collect())
}

fn optional_positive_u64(
    value: Option<&Value>,
) -> Result<Option<u64>, OpenRouterChatDiscoveryError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| catalog_value("catalog.u64")),
    }
}

fn optional_bool(value: Option<&Value>, path: &str) -> Result<bool, OpenRouterChatDiscoveryError> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(catalog_value(path)),
    }
}

fn parse_tool_choice_modes(
    value: Option<&Value>,
    path: &str,
) -> Result<OpenRouterToolChoiceModes, OpenRouterChatDiscoveryError> {
    match value {
        None | Some(Value::Null) => Ok(OpenRouterToolChoiceModes::default()),
        Some(value) => {
            let object = catalog_object(value, path, &[])?;
            Ok(OpenRouterToolChoiceModes {
                none: optional_bool(object.get("none"), &bounded_catalog_path(path, "none"))?,
                auto: optional_bool(object.get("auto"), &bounded_catalog_path(path, "auto"))?,
                required: optional_bool(
                    object.get("required"),
                    &bounded_catalog_path(path, "required"),
                )?,
                function: optional_bool(
                    object.get("function"),
                    &bounded_catalog_path(path, "function"),
                )?,
            })
        }
    }
}

fn parse_reasoning_facts(
    value: Option<&Value>,
) -> Result<Option<OpenRouterReasoningFacts>, OpenRouterChatDiscoveryError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let object = catalog_object(value, "models.data[].reasoning", &[])?;
    let supported_efforts = match object.get("supported_efforts") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_reasoning_efforts(value)?),
    };
    let default_effort = match object.get("default_effort") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            bounded_reasoning_token(Some(value), "models.data[].reasoning.default_effort")?
                .to_owned(),
        ),
    };
    Ok(Some(OpenRouterReasoningFacts {
        supported_efforts,
        default_effort,
        default_enabled: optional_bool(
            object.get("default_enabled"),
            "models.data[].reasoning.default_enabled",
        )?,
        supports_max_tokens: optional_bool(
            object.get("supports_max_tokens"),
            "models.data[].reasoning.supports_max_tokens",
        )?,
        mandatory: optional_bool(object.get("mandatory"), "models.data[].reasoning.mandatory")?,
    }))
}

fn parse_reasoning_efforts(value: &Value) -> Result<Vec<String>, OpenRouterChatDiscoveryError> {
    let efforts = value
        .as_array()
        .ok_or_else(|| catalog_value("models.data[].reasoning.supported_efforts"))?;
    if efforts.len() > MAX_REASONING_EFFORTS {
        return Err(OpenRouterChatDiscoveryError::ResourceLimit);
    }
    let mut result = Vec::with_capacity(efforts.len());
    let mut seen = BTreeSet::new();
    for effort in efforts {
        let effort =
            bounded_reasoning_token(Some(effort), "models.data[].reasoning.supported_efforts[]")?;
        if !seen.insert(effort.to_owned()) {
            return Err(catalog_value("models.data[].reasoning.supported_efforts[]"));
        }
        result.push(effort.to_owned());
    }
    Ok(result)
}

fn bounded_reasoning_token<'a>(
    value: Option<&'a Value>,
    path: &str,
) -> Result<&'a str, OpenRouterChatDiscoveryError> {
    let value = bounded_string(value)?;
    if value.len() > MAX_REASONING_EFFORT_BYTES || !safe_reasoning_token(value) {
        return Err(catalog_value(path));
    }
    Ok(value)
}

fn safe_reasoning_token(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn bounded_parameters(value: &Value) -> Result<BTreeSet<String>, OpenRouterChatDiscoveryError> {
    let parameters = value
        .as_array()
        .ok_or_else(|| catalog_value("catalog.parameters"))?;
    if parameters.len() > MAX_PARAMETERS {
        return Err(OpenRouterChatDiscoveryError::ResourceLimit);
    }
    let mut result = BTreeSet::new();
    for parameter in parameters {
        let parameter = bounded_string(Some(parameter))?;
        if parameter.len() > MAX_PARAMETER_BYTES || !result.insert(parameter.to_owned()) {
            return Err(catalog_value("catalog.parameters[]"));
        }
    }
    Ok(result)
}

/// Upstream catalogs grow additive fields; unknown keys are ignored. Every
/// field the admission decision reads is still type- and bound-checked, and
/// duplicate keys are refused by `parse_unique_json`.
fn catalog_object<'a>(
    value: &'a Value,
    path: &str,
    required: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, OpenRouterChatDiscoveryError> {
    let object = value.as_object().ok_or_else(|| catalog_value(path))?;
    if let Some(key) = required
        .iter()
        .copied()
        .find(|key| !object.contains_key(*key))
    {
        return Err(OpenRouterChatDiscoveryError::MissingCatalogField(
            bounded_catalog_path(path, key),
        ));
    }
    Ok(object)
}

fn catalog_value(path: &str) -> OpenRouterChatDiscoveryError {
    let path: String = path
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '[' | ']'))
        .take(96)
        .collect();
    OpenRouterChatDiscoveryError::InvalidCatalogValue(if path.is_empty() {
        "catalog".to_owned()
    } else {
        path
    })
}

fn bounded_catalog_path(path: &str, key: &str) -> String {
    let path: String = path
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '[' | ']'))
        .take(96)
        .collect();
    let key: String = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        .take(64)
        .collect();
    if path.is_empty() {
        format!("catalog.{key}")
    } else {
        format!("{path}.{key}")
    }
}

fn bounded_string(value: Option<&Value>) -> Result<&str, OpenRouterChatDiscoveryError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| catalog_value("catalog.string"))?;
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(catalog_value("catalog.string"));
    }
    Ok(value)
}

fn safe_endpoint(value: &str) -> bool {
    value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
        && !value.split('/').any(str::is_empty)
}

/// Redaction-safe authenticated discovery failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenRouterChatDiscoveryError {
    /// Literal author/slug was not a permanent exact identity.
    #[error("OpenRouter Chat discovery literal is invalid")]
    InvalidLiteral,
    /// Operation bounds or freshness policy were invalid.
    #[error("OpenRouter Chat discovery policy is invalid")]
    InvalidPolicy,
    /// JSON, status, media type, fields, or endpoint facts were invalid.
    #[error("OpenRouter Chat discovery response is invalid: catalog envelope")]
    InvalidResponse,
    /// Authenticated catalog GET returned a non-OK status.
    #[error("OpenRouter Chat discovery response is invalid: unexpected HTTP status")]
    UnexpectedStatus,
    /// Authenticated catalog GET was not `application/json`.
    #[error("OpenRouter Chat discovery response is invalid: unexpected media type")]
    UnexpectedMediaType,
    /// Authenticated catalog GET body was empty.
    #[error("OpenRouter Chat discovery response is invalid: empty body")]
    EmptyBody,
    /// Authenticated catalog GET body was not unique bounded JSON.
    #[error("OpenRouter Chat discovery response is invalid: catalog JSON")]
    InvalidJson,
    /// A catalog value at a bounded path was the wrong JSON shape or range.
    #[error("OpenRouter Chat discovery response is invalid: catalog value at {0}")]
    InvalidCatalogValue(String),
    /// Catalog JSON omitted a required observation field.
    #[error("OpenRouter Chat discovery response is invalid: missing catalog field at {0}")]
    MissingCatalogField(String),
    /// The literal model was absent or duplicated.
    #[error("OpenRouter Chat discovery model is absent or ambiguous")]
    AmbiguousModel,
    /// A configured or catalog bound was exceeded.
    #[error("OpenRouter Chat discovery exceeded a resource bound")]
    ResourceLimit,
    /// Authentication failed without exposing credential material.
    #[error(transparent)]
    Authentication(#[from] crate::OpenRouterAuthError),
    /// HTTP request construction failed.
    #[error(transparent)]
    Request(#[from] plakous_http::RequestError),
    /// The bounded transport failed.
    #[error(transparent)]
    Transport(#[from] plakous_http::Failure),
    /// Identity construction failed.
    #[error(transparent)]
    Identity(#[from] plakous_model::IdentityError),
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] plakous_model::DigestError),
    /// Authenticated evidence construction failed closed.
    #[error(transparent)]
    Evidence(#[from] OpenRouterChatBuildError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_listing_preserves_literals_and_order_without_capability_claims()
    -> Result<(), OpenRouterChatDiscoveryError> {
        let value = serde_json::json!({"data": [
            {"id":"vendor/z:free", "future_field": true}, {"id":"vendor/a"}, {"id":"~vendor/latest"}
        ]});
        let models = catalog_model_ids(&value)?;
        assert_eq!(
            models.iter().map(String::as_str).collect::<Vec<_>>(),
            ["vendor/z:free", "vendor/a", "~vendor/latest"]
        );
        assert!(catalog_model_ids(&serde_json::json!({"data":[]}))?.is_empty());
        assert!(catalog_model_ids(&serde_json::json!({"data":[],"future":true}))?.is_empty());
        for invalid in [
            serde_json::json!({"data":[{"id":"vendor/a"},{"id":"vendor/a"}]}),
            serde_json::json!({"data":[{"id":12}]}),
            serde_json::json!({"data":[{}]}),
            serde_json::json!({"data":{}}),
            serde_json::json!({"data": vec![serde_json::json!({"id":"vendor/a"}); MAX_MODELS + 1]}),
        ] {
            assert!(catalog_model_ids(&invalid).is_err());
        }
        assert!(parse_unique_json(br#"{"data":[],"data":[]}"#).is_err());
        Ok(())
    }

    use std::io;
    use std::num::NonZeroU32;
    use std::sync::Arc;

    use plakous_http::{Limits, TlsConfigIdentity};
    use plakous_model::{
        CachePolicy, CapacityCertificate, CapacityRule, ContextAdmissionRequirement,
        ContextContract, ContextEvidenceSource, CountCertificateKind, CredentialSelectionId,
        EvidenceFreshness, LanguageModel, LocalDurabilityDisposition, MeasurementCertificate,
        ModelAdapterProfileId, OutputBoundDisposition, OutputPolicy, ParallelToolDisposition,
        ProtocolId, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
        ProviderProfileRevisionId, ProviderRouteSelection, ProviderStorageDisposition,
        ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
        ReplayDisposition, ResponseStateDisposition, RetentionDisposition, RuntimeModelAuthority,
        SelectedProviderConfiguration, ToolChoice, ToolPresentationMode,
    };
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::{ClientConfig, RootCertStore, ServerConfig};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use crate::{
        CHAT_COMPLETIONS_FAMILY_ID, OpenRouterApiKey, OpenRouterChatLanguageModel,
        OpenRouterChatTarget, OpenRouterOrigin, OpenRouterRetentionPolicy,
        StaticOpenRouterApiKeySource,
    };

    fn custom_context(
        route: &OpenRouterChatRoute,
        maximum_projected_input: u64,
    ) -> Result<ContextContract, Box<dyn std::error::Error>> {
        let authority_binding = RuntimeModelAuthority::authority_class_for(
            route.identity().account_binding(),
            route.identity().credential_authority_binding(),
        )?;
        let declaration = Digest::sha256(
            DigestDomain::ProviderSemanticEvidence,
            1,
            &[
                b"test-caller/openrouter-chat-context-v1",
                route.identity().fingerprint().as_bytes(),
                &maximum_projected_input.to_be_bytes(),
            ],
        )?;
        let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
        let revision = "test-caller-openrouter-chat-context-v1";
        let capacity = CapacityCertificate::custom_envelope(
            "test-caller-openrouter-chat-capacity",
            revision,
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(maximum_projected_input),
                maximum_projected_input,
                auto_compact_input: None,
            },
            authority_binding,
            freshness,
            declaration,
            revision,
        )?;
        let measurement = MeasurementCertificate::new(
            "test-caller-openrouter-chat-wire-bound",
            revision,
            "complete-lowered-chat-body-utf8-byte-upper-bound",
            revision,
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound { declaration },
            Some(authority_binding),
            freshness,
        )?;
        Ok(ContextContract::new(
            ProtocolId::new(CHAT_COMPLETIONS_FAMILY_ID)?,
            "openrouter-chat-custom-accounting-v1",
            "openrouter-chat-canonical-v1",
            "openrouter-chat-output-reservation-v1",
            capacity,
            vec![measurement],
        )?)
    }

    fn test_route() -> Result<OpenRouterChatRoute, Box<dyn std::error::Error>> {
        let source = Arc::new(StaticOpenRouterApiKeySource::new(
            OpenRouterApiKey::new("test-catalog-key")?,
            "test-account",
            "test-authority",
        )?);
        Ok(OpenRouterChatRoute::new_local_fixture_with_retention(
            source,
            OpenRouterOrigin::new("https://127.0.0.1:18443")?,
            OpenRouterRetentionPolicy::strict(),
        )?)
    }

    fn selected_for(
        target: &OpenRouterChatTarget,
    ) -> Result<SelectedProviderConfiguration, Box<dyn std::error::Error>> {
        let descriptor = target.descriptor();
        let revision = ProviderProfileRevisionId::new("openrouter-chat-custom-v1")?;
        Ok(SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                descriptor.protocol().clone(),
                descriptor.route().clone(),
                descriptor.profile().clone(),
                revision.clone(),
                ModelAdapterProfileId::new("openrouter-chat-api-key")?,
                ProviderProfileRevisionId::new("openrouter-chat-auth-v1")?,
                ProviderAuthorityClassId::new("custom-openrouter-chat")?,
                CredentialSelectionId::new("openrouter-chat-key")?,
                descriptor.requested_model().clone(),
            ),
            ProviderBehaviorConfiguration {
                processing_tier: plakous_model::ProcessingTierDisposition::Standard,
                tool_mode: ToolPresentationMode::Direct,
                reasoning_effort: ReasoningEffortDisposition::Disabled,
                reasoning_summary: ReasoningSummaryDisposition::Omit,
                reasoning_include: ReasoningIncludeDisposition::Omit,
                replay: ReplayDisposition::DropUnreplayableReasoning,
                cache: CachePolicy::Disabled,
                tools: Vec::new(),
                hosted_tools: Vec::new(),
                tool_choice: ToolChoice::None,
                parallel_tools: ParallelToolDisposition::Disabled,
                output: OutputPolicy::default(),
                output_bound: OutputBoundDisposition::Enforced(
                    NonZeroU32::new(1024).ok_or("nonzero output")?,
                ),
                storage: ProviderStorageDisposition::DisabledOnWire,
                retention: RetentionDisposition::RouteContract {
                    profile: descriptor.profile().clone(),
                    revision,
                },
                response_state: ResponseStateDisposition::Stateless,
                context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
                local_durability: LocalDurabilityDisposition::Ephemeral,
            },
        )?)
    }

    #[test]
    fn literal_paths_are_exact_and_variants_are_encoded_without_privilege()
    -> Result<(), Box<dyn std::error::Error>> {
        let (author, slug, model) = exact_literal("vendor/model-v1:free")?;
        assert_eq!(author, "vendor");
        assert_eq!(model.as_str(), "vendor/model-v1:free");
        assert_eq!(encode_path_component(slug), "model-v1%3Afree");
        let (_, slug, _) = exact_literal("vendor/model-v1:any-variant")?;
        assert_eq!(encode_path_component(slug), "model-v1%3Aany-variant");
        for invalid in ["vendor/a/b", "Vendor/model", "./model", "vendor/.."] {
            assert!(exact_literal(invalid).is_err());
        }
        Ok(())
    }

    #[test]
    fn duplicate_and_unknown_json_fail_closed() {
        assert!(parse_unique_json(br#"{"data":[],"data":[]}"#).is_err());
        match parse_unique_json(b"{") {
            Err(OpenRouterChatDiscoveryError::InvalidJson) => {}
            other => panic!("expected named catalog JSON, got {other:?}"),
        }
        assert!(
            OpenRouterChatDiscoveryError::UnexpectedStatus
                .to_string()
                .contains("unexpected HTTP status")
        );
        assert!(
            OpenRouterChatDiscoveryError::UnexpectedMediaType
                .to_string()
                .contains("unexpected media type")
        );
        let not_object = serde_json::json!([]);
        match validate_models_document(&not_object, "vendor/model") {
            Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(path)) => {
                assert_eq!(path, "models");
            }
            other => panic!(
                "expected named catalog value, got {}",
                match other {
                    Ok(_) => "ok",
                    Err(_) => "other error",
                }
            ),
        }
        let evolving_neighbor = serde_json::json!({"data": [
            {"id": "other/model", "future_neighbor_field": true},
            {"id": "vendor/model", "canonical_slug": "vendor/model", "context_length": 1024,
             "supported_parameters": ["tools"]}
        ]});
        assert!(validate_models_document(&evolving_neighbor, "vendor/model").is_ok());
        let evolving_selected = serde_json::json!({"data": [
            {"id": "vendor/model", "canonical_slug": "vendor/model", "context_length": 1024,
             "supported_parameters": ["tools"], "future_selected_field": true}
        ]});
        assert!(validate_models_document(&evolving_selected, "vendor/model").is_ok());
        let variant = serde_json::json!({"data": [
            {"id": "vendor/model:free", "canonical_slug": "vendor/model-20260824",
             "context_length": 1024, "supported_parameters": ["tools"]}
        ]});
        assert!(validate_models_document(&variant, "vendor/model:free").is_ok());
    }

    #[test]
    fn endpoints_parse_supports_tool_choice_capability_object()
    -> Result<(), Box<dyn std::error::Error>> {
        let endpoint = |tool_choice: Value| {
            serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
                "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],
                "supports_tool_choice": tool_choice
            }]}})
        };
        let parsed = validate_endpoints_document(
            &endpoint(serde_json::json!({
                "none": true,
                "auto": false,
                "required": true,
                "function": false
            })),
            "vendor/model",
        )?;
        let candidate = &parsed[0];
        assert_eq!(
            candidate.tool_choice(),
            OpenRouterToolChoiceModes {
                none: true,
                auto: false,
                required: true,
                function: false,
            }
        );

        let missing_keys = validate_endpoints_document(
            &endpoint(serde_json::json!({"auto": true})),
            "vendor/model",
        )?;
        assert_eq!(
            missing_keys[0].tool_choice(),
            OpenRouterToolChoiceModes {
                none: false,
                auto: true,
                required: false,
                function: false,
            }
        );

        let absent = validate_endpoints_document(
            &serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
                "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"]
            }]}}),
            "vendor/model",
        )?;
        assert_eq!(
            absent[0].tool_choice(),
            OpenRouterToolChoiceModes::default()
        );

        match validate_endpoints_document(&endpoint(serde_json::json!(true)), "vendor/model") {
            Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(path)) => {
                assert_eq!(path, "endpoints.data.endpoints[].supports_tool_choice");
            }
            other => panic!("expected boolean rejection, got {other:?}"),
        }

        let future_mode = validate_endpoints_document(
            &endpoint(serde_json::json!({"none": true, "future_mode": true})),
            "vendor/model",
        )?;
        assert_eq!(
            future_mode[0].tool_choice(),
            OpenRouterToolChoiceModes {
                none: true,
                ..OpenRouterToolChoiceModes::default()
            }
        );
        Ok(())
    }

    #[test]
    fn models_parse_reasoning_capability_object() -> Result<(), Box<dyn std::error::Error>> {
        let model = |reasoning: Option<Value>| {
            let mut row = serde_json::json!({
                "id": "vendor/model",
                "canonical_slug": "vendor/model",
                "context_length": 4096,
                "supported_parameters": ["tools"]
            });
            if let Some(reasoning) = reasoning {
                row["reasoning"] = reasoning;
            }
            serde_json::json!({"data": [row]})
        };

        let with_efforts = validate_models_document(
            &model(Some(serde_json::json!({
                "supported_efforts": ["xhigh", "high", "medium", "low", "minimal", "none"],
                "default_effort": "medium",
                "default_enabled": true,
                "supports_max_tokens": false,
                "mandatory": false
            }))),
            "vendor/model",
        )?;
        let reasoning = with_efforts
            .reasoning
            .as_ref()
            .ok_or("reasoning object should be present")?;
        assert_eq!(
            reasoning.supported_efforts(),
            Some(
                [
                    "xhigh".to_owned(),
                    "high".to_owned(),
                    "medium".to_owned(),
                    "low".to_owned(),
                    "minimal".to_owned(),
                    "none".to_owned(),
                ]
                .as_slice()
            )
        );
        assert_eq!(reasoning.default_effort.as_deref(), Some("medium"));
        assert!(reasoning.default_enabled());
        assert!(!reasoning.supports_max_tokens);
        assert!(!reasoning.mandatory());

        let max_tokens_only = validate_models_document(
            &model(Some(serde_json::json!({
                "supports_max_tokens": true,
                "default_enabled": true,
                "mandatory": false
            }))),
            "vendor/model",
        )?;
        let reasoning = max_tokens_only
            .reasoning
            .as_ref()
            .ok_or("reasoning object should be present")?;
        assert!(reasoning.supported_efforts().is_none());
        assert!(reasoning.supports_max_tokens);

        let absent = validate_models_document(&model(None), "vendor/model")?;
        assert!(absent.reasoning.is_none());

        let unknown_only = validate_models_document(
            &model(Some(serde_json::json!({"supported": true}))),
            "vendor/model",
        )?;
        let reasoning = unknown_only
            .reasoning
            .as_ref()
            .ok_or("reasoning object should be present")?;
        assert!(reasoning.supported_efforts().is_none());
        assert!(!reasoning.supports_max_tokens);

        let too_many = (0..17)
            .map(|index| format!("effort-{index}"))
            .collect::<Vec<_>>();
        assert!(matches!(
            validate_models_document(
                &model(Some(serde_json::json!({"supported_efforts": too_many}))),
                "vendor/model",
            ),
            Err(OpenRouterChatDiscoveryError::ResourceLimit)
        ));

        assert!(matches!(
            validate_models_document(
                &model(Some(serde_json::json!({"supported_efforts": ["high", "high"]}))),
                "vendor/model",
            ),
            Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(path))
                if path == "models.data[].reasoning.supported_efforts[]"
        ));
        Ok(())
    }

    #[test]
    fn snapshot_from_facts_rotates_on_tool_choice_drift() -> Result<(), Box<dyn std::error::Error>>
    {
        let models = parse_unique_json(
            br#"{"data":[{"id":"vendor/model","canonical_slug":"vendor/model","context_length":4096,"supported_parameters":["tools"]}]}"#,
        )?;
        let none_true = parse_unique_json(
            br#"{"data":{"id":"vendor/model","endpoints":[{"tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],"supports_tool_choice":{"none":true,"auto":true,"required":false,"function":false}}]}}"#,
        )?;
        let none_false = parse_unique_json(
            br#"{"data":{"id":"vendor/model","endpoints":[{"tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],"supports_tool_choice":{"none":false,"auto":true,"required":false,"function":false}}]}}"#,
        )?;
        let facts = validate_models_document(&models, "vendor/model")?;
        let with_none = validate_endpoints_document(&none_true, "vendor/model")?;
        let without_none = validate_endpoints_document(&none_false, "vendor/model")?;
        let route = "stable-route";
        let first = snapshot_from_facts(route, "vendor/model", &facts, &with_none)?;
        let drifted = snapshot_from_facts(route, "vendor/model", &facts, &without_none)?;
        assert_ne!(first, drifted);
        Ok(())
    }

    #[test]
    fn snapshot_from_facts_rotates_on_reasoning_and_endpoint_capability_drift()
    -> Result<(), Box<dyn std::error::Error>> {
        let base_models = parse_unique_json(
            br#"{"data":[{"id":"vendor/model","canonical_slug":"vendor/model","context_length":4096,"supported_parameters":["tools"]}]}"#,
        )?;
        let base_endpoints = parse_unique_json(
            br#"{"data":{"id":"vendor/model","endpoints":[{"tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],"supports_implicit_caching":false}]}}"#,
        )?;
        let route = "stable-route";
        let base_facts = validate_models_document(&base_models, "vendor/model")?;
        let base_candidates = validate_endpoints_document(&base_endpoints, "vendor/model")?;
        let base = snapshot_from_facts(route, "vendor/model", &base_facts, &base_candidates)?;

        let model_with_reasoning = |reasoning: Value| {
            serde_json::json!({
                "data": [{
                    "id": "vendor/model",
                    "canonical_slug": "vendor/model",
                    "context_length": 4096,
                    "supported_parameters": ["tools"],
                    "reasoning": reasoning
                }]
            })
        };
        for reasoning in [
            serde_json::json!({"supported_efforts": ["low", "high"]}),
            serde_json::json!({"supported_efforts": ["high", "low"]}),
            serde_json::json!({"default_effort": "medium"}),
            serde_json::json!({"default_effort": "low"}),
            serde_json::json!({"default_enabled": true}),
            serde_json::json!({"default_enabled": false}),
            serde_json::json!({"supports_max_tokens": true}),
            serde_json::json!({"supports_max_tokens": false}),
            serde_json::json!({"mandatory": true}),
            serde_json::json!({"mandatory": false}),
        ] {
            let facts = validate_models_document(&model_with_reasoning(reasoning), "vendor/model")?;
            let identity = snapshot_from_facts(route, "vendor/model", &facts, &base_candidates)?;
            assert_ne!(base, identity);
        }

        let caching_endpoint = serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
            "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],
            "supports_implicit_caching": true
        }]}});
        let candidates = validate_endpoints_document(&caching_endpoint, "vendor/model")?;
        let identity = snapshot_from_facts(route, "vendor/model", &base_facts, &candidates)?;
        assert_ne!(base, identity);
        Ok(())
    }

    #[test]
    fn duplicate_endpoints_coalesce_only_equal_authority() -> Result<(), Box<dyn std::error::Error>>
    {
        let base = serde_json::json!({
            "tag": "b/model", "provider_name": "Beta",
            "supported_parameters": ["tools", "max_tokens"],
            "context_length": 4096, "max_prompt_tokens": 3072,
            "max_completion_tokens": 1024,
            "supports_tool_choice": {"auto": true},
            "supports_implicit_caching": false,
            "uptime_last_1d": 99.1
        });
        let document = |rows: Vec<Value>| {
            serde_json::json!({
                "data": {"id": "vendor/model", "endpoints": rows}
            })
        };
        let mut duplicate = base.clone();
        duplicate["uptime_last_1d"] = serde_json::json!(99.9);
        duplicate["supported_parameters"] = serde_json::json!(["max_tokens", "tools"]);
        let mut other = base.clone();
        other["tag"] = serde_json::json!("a/model");
        let expected = validate_endpoints_document(
            &document(vec![other.clone(), base.clone()]),
            "vendor/model",
        )?;
        assert_eq!(expected[0].endpoint(), "a/model");
        assert_eq!(expected[1].endpoint(), "b/model");
        for rows in [
            vec![base.clone(), other.clone(), duplicate.clone()],
            vec![duplicate.clone(), base.clone(), other.clone()],
        ] {
            assert_eq!(
                validate_endpoints_document(&document(rows), "vendor/model")?,
                expected
            );
        }
        for (field, conflict) in [
            ("provider_name", serde_json::json!("Other")),
            ("supported_parameters", serde_json::json!(["tools"])),
            ("context_length", serde_json::json!(8192)),
            ("max_prompt_tokens", serde_json::json!(2048)),
            ("max_completion_tokens", serde_json::json!(512)),
            ("supports_tool_choice", serde_json::json!({"auto": false})),
            ("supports_implicit_caching", serde_json::json!(true)),
        ] {
            let mut changed = base.clone();
            changed[field] = conflict;
            for rows in [
                vec![base.clone(), changed.clone()],
                vec![changed, base.clone()],
            ] {
                assert!(
                    matches!(
                        validate_endpoints_document(&document(rows), "vendor/model"),
                        Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(_))
                    ),
                    "conflicting {field}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn ambiguous_duplicate_tags_drop_only_that_endpoint() -> Result<(), Box<dyn std::error::Error>>
    {
        let kept = serde_json::json!({
            "tag": "a/model", "provider_name": "Alpha",
            "supported_parameters": ["tools", "max_tokens"],
            "context_length": 8192,
            "max_prompt_tokens": 6144,
            "max_completion_tokens": 2048,
            "supports_tool_choice": {"auto": true},
            "supports_implicit_caching": false,
            "uptime_last_30m": 98.0
        });
        let mut kept_repeat = kept.clone();
        kept_repeat["uptime_last_30m"] = serde_json::json!(97.0);
        kept_repeat["supported_parameters"] = serde_json::json!(["max_tokens", "tools"]);
        let other = serde_json::json!({
            "tag": "z/model", "provider_name": "Zed",
            "supported_parameters": ["tools"]
        });
        let base_conflict = serde_json::json!({
            "tag": "baseten/fp8", "provider_name": "Baseten",
            "supported_parameters": ["tools", "response_format"],
            "context_length": 4096,
            "max_prompt_tokens": 3072,
            "max_completion_tokens": 1024,
            "supports_tool_choice": {"auto": true, "none": true},
            "supports_implicit_caching": false
        });
        let document = |rows: Vec<Value>| serde_json::json!({"data": {"id": "vendor/model", "endpoints": rows}});
        let tags = |candidates: &[OpenRouterChatEndpointCandidate]| {
            candidates
                .iter()
                .map(|candidate| candidate.endpoint().to_owned())
                .collect::<Vec<_>>()
        };
        let facts = ModelFacts {
            supported_parameters: BTreeSet::from(["tools".to_owned()]),
            reasoning: None,
            context_length: 8192,
        };
        let admitted_without_conflict = validate_endpoints_document(
            &document(vec![other.clone(), kept.clone(), kept_repeat.clone()]),
            "vendor/model",
        )?;
        assert_eq!(tags(&admitted_without_conflict), ["a/model", "z/model"]);
        let clean_identity = snapshot_from_facts(
            "stable-route",
            "vendor/model",
            &facts,
            &admitted_without_conflict,
        )?;

        for (field, conflict) in [
            ("provider_name", serde_json::json!("Other")),
            ("supported_parameters", serde_json::json!(["tools"])),
            ("context_length", serde_json::json!(8192)),
            ("max_prompt_tokens", serde_json::json!(2048)),
            ("max_completion_tokens", serde_json::json!(512)),
            ("supports_tool_choice", serde_json::json!({"auto": false})),
            ("supports_implicit_caching", serde_json::json!(true)),
        ] {
            let mut changed = base_conflict.clone();
            changed[field] = conflict;
            // A later row that matches the first must not put the tag back.
            let mut majority = base_conflict.clone();
            majority["uptime_last_30m"] = serde_json::json!(50.0);
            let orders = [
                vec![
                    base_conflict.clone(),
                    changed.clone(),
                    kept.clone(),
                    other.clone(),
                    kept_repeat.clone(),
                    majority.clone(),
                ],
                vec![
                    changed.clone(),
                    other.clone(),
                    majority.clone(),
                    kept_repeat.clone(),
                    base_conflict.clone(),
                    kept.clone(),
                ],
                vec![
                    kept_repeat.clone(),
                    majority,
                    other.clone(),
                    changed.clone(),
                    kept.clone(),
                    base_conflict.clone(),
                ],
            ];
            for rows in orders {
                let candidates = validate_endpoints_document(&document(rows), "vendor/model")?;
                assert_eq!(
                    tags(&candidates),
                    ["a/model", "z/model"],
                    "conflicting {field}"
                );
                let identity =
                    snapshot_from_facts("stable-route", "vendor/model", &facts, &candidates)?;
                assert_eq!(identity, clean_identity, "conflicting {field}");
            }
            for winner in [base_conflict.clone(), changed] {
                let picked = validate_endpoints_document(
                    &document(vec![winner, kept.clone(), other.clone()]),
                    "vendor/model",
                )?;
                let picked_identity =
                    snapshot_from_facts("stable-route", "vendor/model", &facts, &picked)?;
                assert_ne!(picked_identity, clean_identity, "winner for {field}");
            }
        }

        let mut only_changed = base_conflict.clone();
        only_changed["supported_parameters"] = serde_json::json!(["tools"]);
        for rows in [
            vec![base_conflict.clone(), only_changed.clone()],
            vec![only_changed.clone(), base_conflict.clone()],
        ] {
            assert!(
                matches!(
                    validate_endpoints_document(&document(rows), "vendor/model"),
                    Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(path))
                        if path == "endpoints.data.endpoints[].tag"
                ),
                "all-conflicting"
            );
        }

        let mut second = other.clone();
        second["provider_name"] = serde_json::json!("OtherZed");
        let candidates = validate_endpoints_document(
            &document(vec![
                base_conflict,
                only_changed,
                second,
                other,
                kept.clone(),
            ]),
            "vendor/model",
        )?;
        assert_eq!(tags(&candidates), ["a/model"]);

        let mut unsafe_row = kept_repeat.clone();
        unsafe_row["tag"] = serde_json::json!("bad tag");
        assert!(matches!(
            validate_endpoints_document(
                &document(vec![kept, unsafe_row, kept_repeat]),
                "vendor/model"
            ),
            Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(_))
        ));
        Ok(())
    }

    #[test]
    fn duplicate_endpoints_still_validate_every_row() {
        let base = serde_json::json!({
            "tag": "a/model", "provider_name": "Alpha", "supported_parameters": ["tools"]
        });
        for (field, malformed) in [
            ("supported_parameters", serde_json::json!("tools")),
            ("max_completion_tokens", serde_json::json!(0)),
            ("supports_tool_choice", serde_json::json!({"auto": "true"})),
            ("supports_implicit_caching", serde_json::json!("false")),
        ] {
            let mut duplicate = base.clone();
            duplicate[field] = malformed;
            for rows in [
                vec![base.clone(), duplicate.clone()],
                vec![duplicate, base.clone()],
            ] {
                let document =
                    serde_json::json!({"data": {"id": "vendor/model", "endpoints": rows}});
                assert!(
                    validate_endpoints_document(&document, "vendor/model").is_err(),
                    "malformed {field}"
                );
            }
        }
    }

    #[test]
    fn endpoints_admit_observed_supports_voice_cloning() {
        let ok = serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
            "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],
            "supports_voice_cloning": true
        }]}});
        assert!(validate_endpoints_document(&ok, "vendor/model").is_ok());
    }

    #[test]
    fn additive_upstream_fields_are_ignored_but_known_fields_stay_checked()
    -> Result<(), Box<dyn std::error::Error>> {
        // Shape of the endpoints document that first carried
        // `supports_image_reference`, plus fields at every nesting level.
        let document = serde_json::json!({"future_root": 1, "data": {
            "id": "vendor/model", "future_data": {"x": [1]},
            "endpoints": [{
                "tag": "a/model", "provider_name": "Alpha",
                "supported_parameters": ["tools", "tool_choice"],
                "context_length": 4096,
                "supports_image_reference": false,
                "supports_tool_choice": {"auto": true, "future_mode": true},
                "future_endpoint_field": {"nested": true}
            }]
        }});
        let endpoints = validate_endpoints_document(&document, "vendor/model")?;
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].context_length, Some(4096));
        assert!(endpoints[0].tool_choice().auto);

        let models = serde_json::json!({"future_root": true, "data": [{
            "id": "vendor/model", "canonical_slug": "vendor/model", "context_length": 1024,
            "supported_parameters": ["tools"], "future_selected_field": {"a": 1},
            "reasoning": {"supported_efforts": ["low"], "future_reasoning": 1}
        }]});
        let facts = validate_models_document(&models, "vendor/model")?;
        assert_eq!(facts.context_length, 1024);
        assert_eq!(catalog_model_ids(&models)?, vec!["vendor/model".to_owned()]);

        // A known field with the wrong type is still refused.
        let mut wrong_type = document.clone();
        wrong_type["data"]["endpoints"][0]["context_length"] = serde_json::json!("4096");
        assert!(matches!(
            validate_endpoints_document(&wrong_type, "vendor/model"),
            Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(_))
        ));
        Ok(())
    }

    #[test]
    fn endpoints_admit_observed_perf_last_30m_by_workload() {
        let ok = serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
            "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],
            "perf_last_30m_by_workload":{"chat":{"p50":1.0}}
        }]}});
        assert!(validate_endpoints_document(&ok, "vendor/model").is_ok());
    }

    #[test]
    fn snapshot_from_facts_ignores_raw_receipt_shape_and_rotates_on_drift()
    -> Result<(), Box<dyn std::error::Error>> {
        let first_models = parse_unique_json(
            br#"{"data":[{"id":"vendor/model","canonical_slug":"vendor/model","context_length":4096,"supported_parameters":["tool_choice","tools"]}],"links":{},"total_count":1}"#,
        )?;
        let reordered_models = parse_unique_json(
            br#"{ "total_count": 1, "data": [ { "supported_parameters": ["tools", "tool_choice"], "context_length": 4096, "canonical_slug": "vendor/model", "id": "vendor/model" } ], "links": {} }"#,
        )?;
        let endpoints = parse_unique_json(
            br#"{"data":{"id":"vendor/model","endpoints":[{"tag":"z/model","provider_name":"Zed","supported_parameters":["tools"]},{"tag":"a/model","provider_name":"Alpha","supported_parameters":["tools","tool_choice"]}]}}"#,
        )?;
        let reordered_endpoints = parse_unique_json(
            br#"{"data":{"endpoints":[{"supported_parameters":["tool_choice","tools"],"provider_name":"Alpha","tag":"a/model"},{"supported_parameters":["tools"],"provider_name":"Zed","tag":"z/model"}],"id":"vendor/model"}}"#,
        )?;
        let first_facts = validate_models_document(&first_models, "vendor/model")?;
        let reordered_facts = validate_models_document(&reordered_models, "vendor/model")?;
        let candidates = validate_endpoints_document(&endpoints, "vendor/model")?;
        let reordered_candidates =
            validate_endpoints_document(&reordered_endpoints, "vendor/model")?;
        let route = "stable-route";
        let first = snapshot_from_facts(route, "vendor/model", &first_facts, &candidates)?;
        let reordered = snapshot_from_facts(route, "vendor/model", &reordered_facts, &candidates)?;
        assert_eq!(first, reordered);
        let endpoint_reordered = snapshot_from_facts(
            route,
            "vendor/model",
            &reordered_facts,
            &reordered_candidates,
        )?;
        assert_eq!(first, endpoint_reordered);

        let drifted_models = parse_unique_json(
            br#"{"data":[{"id":"vendor/model","canonical_slug":"vendor/model","context_length":4095,"supported_parameters":["tools","tool_choice"]}]}"#,
        )?;
        let drifted_facts = validate_models_document(&drifted_models, "vendor/model")?;
        let drifted = snapshot_from_facts(route, "vendor/model", &drifted_facts, &candidates)?;
        assert_ne!(first, drifted);
        Ok(())
    }

    #[test]
    fn chat_configuration_rejects_explicit_tool_choice() -> Result<(), Box<dyn std::error::Error>> {
        let route = test_route()?;
        let now = SystemClock.now();
        let evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
            ModelId::new("vendor/model")?,
            route.identity(),
            vec![OpenRouterChatEndpointCandidate {
                endpoint: "provider/model".to_owned(),
                provider_name: "Provider".to_owned(),
                supported_parameters: BTreeSet::from(
                    ["tools", "max_tokens", "tool_choice"].map(str::to_owned),
                ),
                context_length: Some(4096),
                max_prompt_tokens: Some(3072),
                max_completion_tokens: Some(1024),
                tool_choice: OpenRouterToolChoiceModes {
                    required: true,
                    ..OpenRouterToolChoiceModes::default()
                },
                supports_implicit_caching: false,
            }],
            BTreeSet::from(["tools", "max_tokens", "tool_choice"].map(str::to_owned)),
            None,
            4096,
            now,
            Timestamp::new(now.seconds() + 3600, now.nanoseconds())?,
        )?;
        let contract = custom_context(&route, 2048)?;
        let target = OpenRouterChatTarget::gateway_managed(
            route.clone(),
            evidence.clone(),
            contract.clone(),
        )?;
        let selected = selected_for(&target)?;
        let mut behavior = selected.behavior().clone();
        behavior.tool_choice = ToolChoice::Required;
        let selected = SelectedProviderConfiguration::new(selected.route().clone(), behavior)?;
        assert!(matches!(
            OpenRouterChatLanguageModel::configured_gateway_managed(
                route,
                evidence,
                contract,
                selected,
                Client::new(Limits::default())?
            ),
            Err(OpenRouterChatBuildError::Configuration)
        ));
        Ok(())
    }

    #[test]
    fn time_only_rediscovery_preserves_target_and_effective_configuration_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let route = test_route()?;
        let model = ModelId::new("vendor/model")?;
        let candidates = vec![OpenRouterChatEndpointCandidate {
            endpoint: "provider/model".to_owned(),
            provider_name: "Provider".to_owned(),
            supported_parameters: BTreeSet::from(["tools".to_owned()]),
            context_length: Some(4096),
            max_prompt_tokens: Some(3072),
            max_completion_tokens: Some(1024),
            tool_choice: OpenRouterToolChoiceModes::default(),
            supports_implicit_caching: false,
        }];
        let parameters = BTreeSet::from(["tools".to_owned()]);
        let now = SystemClock.now();
        let first_observed = Timestamp::new(now.seconds().saturating_sub(20), now.nanoseconds())?;
        let second_observed = Timestamp::new(now.seconds().saturating_sub(10), now.nanoseconds())?;
        let first_expiry = Timestamp::new(now.seconds() + 3600, now.nanoseconds())?;
        let second_expiry = Timestamp::new(now.seconds() + 7200, now.nanoseconds())?;
        let first_evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
            model.clone(),
            route.identity(),
            candidates.clone(),
            parameters.clone(),
            None,
            4096,
            first_observed,
            first_expiry,
        )?;
        let second_evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
            model,
            route.identity(),
            candidates,
            parameters,
            None,
            4096,
            second_observed,
            second_expiry,
        )?;
        let context = custom_context(&route, 2048)?;
        let first_target = OpenRouterChatTarget::gateway_managed(
            route.clone(),
            first_evidence.clone(),
            context.clone(),
        )?;
        let second_target = OpenRouterChatTarget::gateway_managed(
            route.clone(),
            second_evidence.clone(),
            context.clone(),
        )?;
        assert_eq!(
            first_target.target_identity(),
            second_target.target_identity()
        );
        assert_eq!(first_target.descriptor(), second_target.descriptor());
        let selected = selected_for(&first_target)?;
        let first = OpenRouterChatLanguageModel::configured_gateway_managed(
            route.clone(),
            first_evidence,
            context.clone(),
            selected.clone(),
            Client::new(Limits::default())?,
        )?;
        let second = OpenRouterChatLanguageModel::configured_gateway_managed(
            route,
            second_evidence,
            context,
            selected,
            Client::new(Limits::default())?,
        )?;
        assert_eq!(
            first.configuration().effective_identity(),
            second.configuration().effective_identity()
        );
        Ok(())
    }

    #[test]
    fn equal_endpoint_catalog_drift_preserves_chat_admission()
    -> Result<(), Box<dyn std::error::Error>> {
        let route = test_route()?;
        let document = serde_json::json!({"data": {"id": "vendor/model", "endpoints": [{
            "tag": "first/model", "provider_name": "First",
            "supported_parameters": ["tools", "max_tokens"],
            "context_length": 4096, "max_prompt_tokens": 3072,
            "max_completion_tokens": 1024
        }]}});
        let mut added = document.clone();
        let mut row = document["data"]["endpoints"][0].clone();
        row["tag"] = serde_json::json!("second/model");
        row["provider_name"] = serde_json::json!("Second");
        added["data"]["endpoints"]
            .as_array_mut()
            .ok_or("rows")?
            .push(row);
        let facts = ModelFacts {
            supported_parameters: BTreeSet::from(["tools".to_owned(), "max_tokens".to_owned()]),
            reasoning: None,
            context_length: 4096,
        };
        let now = SystemClock.now();
        let mut identities = Vec::new();
        let mut snapshots = Vec::new();
        for document in [&document, &added, &document] {
            let candidates = validate_endpoints_document(document, "vendor/model")?;
            let snapshot = snapshot_from_facts(
                route.identity().fingerprint(),
                "vendor/model",
                &facts,
                &candidates,
            )?;
            snapshots.push(snapshot);
            let evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
                ModelId::new("vendor/model")?,
                route.identity(),
                candidates,
                facts.supported_parameters.clone(),
                None,
                4096,
                now,
                Timestamp::new(now.seconds() + 3600, now.nanoseconds())?,
            )?;
            let target = OpenRouterChatTarget::gateway_managed(
                route.clone(),
                evidence,
                custom_context(&route, 2048)?,
            )?;
            identities.push(target.target_identity());
        }
        assert_ne!(snapshots[0], snapshots[1]);
        assert_eq!(snapshots[0], snapshots[2]);
        assert_eq!(identities[0], identities[1]);
        assert_eq!(identities[0], identities[2]);
        Ok(())
    }

    #[test]
    fn discovery_constrains_but_does_not_own_custom_context()
    -> Result<(), Box<dyn std::error::Error>> {
        let route = test_route()?;
        let now = SystemClock.now();
        let evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
            ModelId::new("vendor/model")?,
            route.identity(),
            vec![OpenRouterChatEndpointCandidate {
                endpoint: "provider/model".to_owned(),
                provider_name: "Provider".to_owned(),
                supported_parameters: BTreeSet::new(),
                context_length: Some(4096),
                max_prompt_tokens: Some(3072),
                max_completion_tokens: Some(1024),
                tool_choice: OpenRouterToolChoiceModes::default(),
                supports_implicit_caching: false,
            }],
            BTreeSet::new(),
            None,
            4096,
            Timestamp::new(now.seconds().saturating_sub(1), now.nanoseconds())?,
            Timestamp::new(now.seconds() + 3600, now.nanoseconds())?,
        )?;
        let conservative = custom_context(&route, 2048)?;
        let target = OpenRouterChatTarget::gateway_managed(
            route.clone(),
            evidence.clone(),
            conservative.clone(),
        )?;
        assert_eq!(target.descriptor().context_contract(), Some(&conservative));
        let excessive = custom_context(&route, 4097)?;
        assert!(matches!(
            OpenRouterChatTarget::gateway_managed(route, evidence, excessive),
            Err(OpenRouterChatBuildError::ContextExceedsDiscovery)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn exact_discovery_freshness_gates_construction_and_runtime_authority()
    -> Result<(), Box<dyn std::error::Error>> {
        let route = test_route()?;
        let context = custom_context(&route, 2048)?;
        let now = SystemClock.now();
        let candidate = OpenRouterChatEndpointCandidate {
            endpoint: "provider/model".to_owned(),
            provider_name: "Provider".to_owned(),
            supported_parameters: BTreeSet::new(),
            context_length: Some(4096),
            max_prompt_tokens: Some(3072),
            max_completion_tokens: Some(1024),
            tool_choice: OpenRouterToolChoiceModes::default(),
            supports_implicit_caching: false,
        };
        let stale = OpenRouterChatModelEvidence::from_authenticated_discovery(
            ModelId::new("vendor/model")?,
            route.identity(),
            vec![candidate.clone()],
            BTreeSet::new(),
            None,
            4096,
            Timestamp::new(now.seconds().saturating_sub(20), now.nanoseconds())?,
            Timestamp::new(now.seconds().saturating_sub(10), now.nanoseconds())?,
        )?;
        assert!(matches!(
            OpenRouterChatTarget::gateway_managed(route.clone(), stale, context.clone()),
            Err(OpenRouterChatBuildError::StaleEvidence)
        ));

        let expiry = Timestamp::new(now.seconds() + 10, now.nanoseconds())?;
        let fresh = OpenRouterChatModelEvidence::from_authenticated_discovery(
            ModelId::new("vendor/model")?,
            route.identity(),
            vec![candidate],
            BTreeSet::new(),
            None,
            4096,
            Timestamp::new(now.seconds().saturating_sub(1), now.nanoseconds())?,
            expiry,
        )?;
        let target = OpenRouterChatTarget::gateway_managed(route, fresh, context)?;
        let selected = selected_for(&target)?;
        let model = Arc::new(OpenRouterChatLanguageModel::new(
            target,
            selected,
            Client::new(Limits::default())?,
        )?);
        assert!(matches!(
            model.revalidate_runtime_authority(expiry).await,
            Err(plakous_model::RuntimeModelAuthorityError::Stale)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_loopback_discovery_binds_exact_literal_and_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
        let certificate = cert.der().clone();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)?;
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(certificate.to_vec()))?;
        let client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tls_identity = TlsConfigIdentity::from_parts(
            1,
            &[b"openrouter-chat-discovery-test-v1", certificate.as_ref()],
        )?;
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin_text = format!("https://{}", listener.local_addr()?);
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let responses = [
            br#"{"data":[{"id":"vendor/model-v1:free","canonical_slug":"vendor/model-v1:free","context_length":32768,"reasoning":{"supported_efforts":["high","medium","low"],"default_effort":"medium","default_enabled":true,"supports_max_tokens":false,"mandatory":false},"supported_parameters":["tools","tool_choice","reasoning"]}],"links":{},"total_count":1}"#.to_vec(),
            br#"{"data":{"id":"vendor/model-v1:free","endpoints":[{"tag":"provider/model-v1","provider_name":"Provider","context_length":32768,"max_prompt_tokens":28672,"max_completion_tokens":4096,"supported_parameters":["tools","tool_choice"],"supports_tool_choice":{"none":true,"auto":true,"required":false,"function":false},"supports_voice_cloning":false}]}}"#.to_vec(),
        ];
        let responses = responses
            .into_iter()
            .chain([br#"{"data":[{"id":"vendor/model-v1:free"},{"id":"other/model"}]}"#.to_vec()]);
        let peer = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in responses {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).await?;
                    if read == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request head"));
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                stream.write_all(&body).await?;
                stream.shutdown().await?;
                requests.push(String::from_utf8(request).map_err(io::Error::other)?);
            }
            Ok::<_, io::Error>(requests)
        });
        let source = Arc::new(StaticOpenRouterApiKeySource::new(
            OpenRouterApiKey::new("test-catalog-key")?,
            "test-account",
            "test-authority",
        )?);
        let route = OpenRouterChatRoute::new_local_fixture_with_retention(
            source,
            OpenRouterOrigin::new(origin_text)?,
            OpenRouterRetentionPolicy::strict(),
        )?;
        let evidence = route
            .discover_chat_model(
                &client,
                "vendor/model-v1:free",
                OpenRouterChatDiscoveryConfig::default(),
            )
            .await?;
        assert_eq!(evidence.model().as_str(), "vendor/model-v1:free");
        assert_eq!(evidence.context_length(), 32_768);
        assert!(evidence.supported_parameters().contains("tools"));
        let endpoint = evidence
            .endpoints()
            .first()
            .ok_or_else(|| io::Error::other("discovery omitted endpoint"))?;
        assert_eq!(endpoint.context_length(), Some(32_768));
        assert_eq!(endpoint.max_prompt_tokens(), Some(28_672));
        assert_eq!(endpoint.max_completion_tokens(), Some(4_096));
        let context = custom_context(&route, 16_384)?;
        let target = OpenRouterChatTarget::gateway_managed(
            route.clone(),
            evidence.clone(),
            context.clone(),
        )?;
        assert_eq!(
            target.descriptor().integration_class(),
            plakous_model::IntegrationClass::Custom
        );
        let descriptor_context = target
            .descriptor()
            .context_contract()
            .ok_or_else(|| io::Error::other("curated target lacks context"))?;
        assert!(matches!(
            descriptor_context.capacity().evidence_class(),
            plakous_model::ContextEvidenceClass::CustomEnvelope { .. }
        ));
        let selected = selected_for(&target)?;
        let listed = route
            .list_models(&client, OpenRouterChatDiscoveryConfig::default())
            .await?;
        assert_eq!(
            listed.iter().map(String::as_str).collect::<Vec<_>>(),
            ["vendor/model-v1:free", "other/model"]
        );
        let configured = OpenRouterChatLanguageModel::configured_gateway_managed(
            route,
            evidence,
            context,
            selected.clone(),
            client,
        )?;
        assert_eq!(configured.configuration().selected(), &selected);
        assert_eq!(
            configured
                .configuration()
                .effective()
                .descriptor()
                .integration_class(),
            plakous_model::IntegrationClass::Custom
        );
        assert_eq!(
            configured.configuration().selected_identity(),
            selected.identity()
        );
        let requests = peer.await??;
        assert_eq!(requests.len(), 3);
        assert!(requests[2].starts_with("GET /api/v1/models HTTP/1.1\r\n"));
        assert!(requests[0].starts_with("GET /api/v1/models HTTP/1.1\r\n"));
        assert!(
            requests[1]
                .starts_with("GET /api/v1/models/vendor/model-v1%3Afree/endpoints HTTP/1.1\r\n")
        );
        assert!(requests.iter().all(|request| request.lines().any(|line| {
            line.to_ascii_lowercase()
                .starts_with("authorization: bearer ")
        })));
        Ok(())
    }
}
