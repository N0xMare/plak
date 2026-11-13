//! Bounded authenticated OpenRouter catalog listing and exact Chat discovery.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::fmt;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use plakous_http::{
    Client, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request, SensitiveBytes,
    StatusCode, sensitive_header_value,
};
use plakous_model::{Digest, DigestDomain, ModelId, Timestamp};
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

#[cfg(feature = "test-clock")]
mod test_support;

/// Explicit bounds for one two-request authenticated discovery operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenRouterChatDiscoveryConfig {
    /// Per-attempt exchange duration; discovery also has a shared 60-second cap.
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
        let key = self.catalog_credential(&operation, &generation)?;
        let deadline = Instant::now()
            .checked_add(config.operation_timeout)
            .ok_or(OpenRouterChatDiscoveryError::InvalidPolicy)?;
        let deadline = plakous_exchange::lookup::LookupBudget::from_deadline(deadline);
        let body = get_json(
            client,
            Origin::parse(self.identity().origin().as_str())?,
            MODELS_PATH,
            crate::route::bearer_bytes(&key),
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
        let key = self.catalog_credential(&operation, &generation)?;
        let deadline = Instant::now()
            .checked_add(config.operation_timeout)
            .ok_or(OpenRouterChatDiscoveryError::InvalidPolicy)?;
        let deadline = plakous_exchange::lookup::LookupBudget::from_deadline(deadline);
        let origin = Origin::parse(self.identity().origin().as_str())?;
        let models = get_json(
            client,
            origin.clone(),
            MODELS_PATH,
            crate::route::bearer_bytes(&key),
            deadline,
            config.maximum_body_bytes,
        )
        .await?;
        // The catalog decides whether the literal exists before its endpoints
        // are asked for: an absent model is a catalog answer, not a status.
        let model_facts = validate_models_document(&parse_unique_json(&models)?, literal)?;
        let endpoints = get_json(
            client,
            origin,
            &endpoint_path,
            crate::route::bearer_bytes(&key),
            deadline,
            config.maximum_body_bytes,
        )
        .await?;
        let endpoints_value = parse_unique_json(&endpoints)?;
        let candidates = validate_endpoints_document(&endpoints_value, literal)?;
        let observed_at = self.clock.now();
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
            crate::chat::OpenRouterChatDiscovery {
                endpoints: candidates,
                model_supported_parameters: model_facts.supported_parameters,
                reasoning: model_facts.reasoning,
                context_length: model_facts.context_length,
                image_input: model_facts.image_input,
                observed_at,
                expires_at,
            },
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
        || config.evidence_ttl.as_secs() > crate::dialect::OPENROUTER_EVIDENCE_MAX_AGE_SECONDS
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
    deadline: plakous_exchange::lookup::LookupBudget,
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
    let reply = plakous_exchange::lookup::get_json(client, request, deadline, maximum_body_bytes)
        .await
        .map_err(|error| match error {
            plakous_exchange::lookup::LookupError::Transport(failure) => {
                OpenRouterChatDiscoveryError::Transport(failure)
            }
            plakous_exchange::lookup::LookupError::MediaType => {
                OpenRouterChatDiscoveryError::UnexpectedMediaType
            }
            plakous_exchange::lookup::LookupError::BodyLimit => {
                OpenRouterChatDiscoveryError::ResourceLimit
            }
            error => OpenRouterChatDiscoveryError::Lookup(error),
        })?;
    if reply.status != StatusCode::OK {
        return Err(OpenRouterChatDiscoveryError::UnexpectedStatus);
    }
    if reply.body.is_empty() {
        return Err(OpenRouterChatDiscoveryError::EmptyBody);
    }
    Ok(reply.body.to_vec())
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
    image_input: bool,
}

pub(crate) fn catalog_snapshot_identity(
    route_fingerprint: &str,
    literal: &str,
    context_length: u64,
    parameters: &BTreeSet<String>,
    reasoning: Option<&OpenRouterReasoningFacts>,
    image_input: bool,
    candidates: &[OpenRouterChatEndpointCandidate],
) -> Result<Digest, OpenRouterChatDiscoveryError> {
    let mut candidates: Vec<_> = candidates.iter().collect();
    candidates.sort_by_key(|candidate| &candidate.endpoint);
    let snapshot = serde_json::to_vec(&(
        literal,
        context_length,
        parameters,
        reasoning,
        image_input,
        &candidates,
    ))
    .map_err(|_| OpenRouterChatDiscoveryError::InvalidCatalogValue("catalog.identity".into()))?;
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
use plakous_model::Clock as _;

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
        facts.image_input,
        candidates,
    )
}

const OUTPUT_DEFAULT: u32 = 4_096;
const REASONING_OUTPUT_DEFAULT: u32 = 16_384;
const OUTPUT_CEILING: u32 = 32_768;
const OUTPUT_FLOOR: u32 = 4_096;
const OUTPUT_RUNGS: [u32; 4] = [4_096, 8_192, 16_384, 32_768];

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
                crate::chat::OPENROUTER_CHAT_DIALECT_REVISION,
            ),
            Self::Responses => (
                crate::OPENROUTER_RESPONSES_PROTOCOL_ID,
                crate::OPENROUTER_RESPONSES_PROFILE,
                crate::dialect::OPENROUTER_RESPONSES_DIALECT_REVISION,
            ),
            Self::Messages => (
                crate::OPENROUTER_MESSAGES_PROTOCOL_ID,
                crate::messages::PROFILE,
                crate::messages::OPENROUTER_MESSAGES_DIALECT_REVISION,
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
        let output_default = if reasoning
            || endpoints
                .iter()
                .any(|endpoint| endpoint.supported_parameters.contains("reasoning"))
        {
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
            input_ceiling: self.context_length().saturating_sub(output_bound),
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
        let mut safe_input = self.context_length().saturating_sub(output);
        for endpoint in endpoints {
            if endpoint
                .max_completion_tokens
                .is_none_or(|bound| bound < output)
            {
                return Err(OpenRouterChatBuildError::UnprovenCompletionBound);
            }
            let context_input = endpoint
                .context_length
                .map(|context| context.saturating_sub(output));
            let prompt = match (endpoint.max_prompt_tokens, context_input) {
                (Some(prompt), Some(context)) => prompt.min(context),
                (Some(prompt), None) => prompt,
                (None, Some(context)) => context,
                (None, None) => return Err(OpenRouterChatBuildError::UnprovenPromptBound),
            };
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
            &[b"plakous-openrouter/wire-admission-v3", &facts],
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
        let image_input = parse_image_input(row.get("architecture"))?;
        matched = Some(ModelFacts {
            supported_parameters,
            reasoning,
            context_length,
            image_input,
        });
    }
    matched.ok_or(OpenRouterChatDiscoveryError::AbsentModel)
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

/// Whether the model row's `architecture.input_modalities` lists `image`. A
/// row without the field accepts no image input.
fn parse_image_input(value: Option<&Value>) -> Result<bool, OpenRouterChatDiscoveryError> {
    let Some(architecture) = value.filter(|value| !value.is_null()) else {
        return Ok(false);
    };
    let architecture = catalog_object(architecture, "models.data[].architecture", &[])?;
    match architecture.get("input_modalities") {
        None | Some(Value::Null) => Ok(false),
        Some(modalities) => Ok(bounded_parameters(modalities)?.contains("image")),
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
    /// Transient model lookup retries exhausted their bounded budget.
    #[error(transparent)]
    Lookup(plakous_exchange::lookup::LookupError),
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
    /// The authenticated catalog lists no row for the literal model.
    #[error("OpenRouter Chat discovery model is absent from the catalog")]
    AbsentModel,
    /// The literal model was duplicated or its canonical slug was unsafe.
    #[error("OpenRouter Chat discovery model is ambiguous")]
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
mod tests;
