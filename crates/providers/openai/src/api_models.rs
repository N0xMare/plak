//! OpenAI API-key model catalog: the provider's live `GET /v1/models` list
//! (ids only), the bundled reviewed table and the reviewed family rules.
//!
//! `/v1/models` attests that an id exists. It publishes no window, output
//! bound or effort. Those facts come from the bundled table a reviewer
//! checked in (model pages, plus the Codex catalog's per-model
//! `context_window` where a page states no price threshold). Whether Plak
//! drives the id is the family rule: only the closed GPT-6 and GPT-5.6
//! platform profiles are usable, and only under the efforts the model page
//! lists. `ultra` is Codex-client-only and is not an API effort. `none` is
//! an explicit effort; an omitted effort stays refused.

use std::time::Instant;

use plakous_http::{
    Client, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request, SensitiveBytes,
    StatusCode, sensitive_header_value,
};
use plakous_model::{ModelErrorKind, ReasoningEffort};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    CredentialReference, EndpointOrigin, ErrorPhase, OPENAI_PLATFORM_ORIGIN, OpenAiError,
    PlatformCredentialSource, RetryClass,
};

/// Exact Models API path on the Platform origin.
const OPENAI_MODELS_PATH: &str = "/v1/models";
/// Output limit the reviewed GPT-5.x and GPT-6 profiles are admitted under.
pub(crate) const REVIEWED_OUTPUT_BOUND: u32 = 128_000;

const BUNDLED_TABLE: &str = include_str!("api_models.json");
const MAX_MODELS: usize = 2048;
const MAX_ID_BYTES: usize = 128;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const EFFORT_LADDER: &[(&str, ReasoningEffort)] = &[
    ("none", ReasoningEffort::None),
    ("low", ReasoningEffort::Low),
    ("medium", ReasoningEffort::Medium),
    ("high", ReasoningEffort::High),
    ("xhigh", ReasoningEffort::XHigh),
    ("max", ReasoningEffort::Max),
];
/// Closed profiles [`crate::OpenAiPlatformModelFacts`] already drives.
const REVIEWED_PROFILES: &[&str] = &[
    "gpt-6-luna",
    "gpt-6-sol",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
];
const NOT_CODING_MARKERS: &[&str] = &[
    "tts",
    "whisper",
    "dall-e",
    "dalle",
    "embedding",
    "moderation",
    "realtime",
    "sora",
    "audio",
    "transcri",
    "image",
];

/// One `/v1/models` id. The live payload publishes nothing else admission reads.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OpenAiListedModel {
    /// Exact model id.
    pub id: String,
}

/// Parses one `/v1/models` body down to safe ids, in provider order.
///
/// # Errors
/// Returns a protocol error for malformed JSON, a missing id, or a duplicate id.
/// An id that is not a safe token is skipped: fine-tune ids must not fail the
/// whole list.
pub(crate) fn parse_openai_models_page(body: &[u8]) -> Result<Vec<OpenAiListedModel>, OpenAiError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| list_error("is not JSON"))?;
    let rows = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| list_error("has no data array"))?;
    if rows.len() > MAX_MODELS {
        return Err(list_error("exceeds the row bound"));
    }
    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            return Err(list_error("has a missing model id"));
        };
        models.push(OpenAiListedModel { id: id.to_owned() });
    }
    accept_openai_listed_models(&models)
}

/// The same id checks a fetched page uses. Cached rows go through this before
/// classification, so a rewritten cache cannot admit an unsafe or repeated id.
/// An id that is not a safe token is skipped. Duplicate ids and an oversized
/// list fail.
///
/// # Errors
/// Returns a protocol error for a duplicate id or a list over the row bound.
pub fn accept_openai_listed_models(
    models: &[OpenAiListedModel],
) -> Result<Vec<OpenAiListedModel>, OpenAiError> {
    if models.len() > MAX_MODELS {
        return Err(list_error("exceeds the row bound"));
    }
    let mut accepted: Vec<OpenAiListedModel> = Vec::with_capacity(models.len());
    for model in models {
        if !safe_id(&model.id) {
            continue;
        }
        if accepted.iter().any(|seen| seen.id == model.id) {
            return Err(list_error("repeats a model id"));
        }
        accepted.push(model.clone());
    }
    Ok(accepted)
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b':'))
}

fn list_error(reason: &str) -> OpenAiError {
    OpenAiError::new(
        ErrorPhase::Request,
        ModelErrorKind::InvalidRequest,
        RetryClass::Never,
        format!("OpenAI model list {reason}"),
    )
}

fn table_error() -> OpenAiError {
    OpenAiError::new(
        ErrorPhase::Request,
        ModelErrorKind::InvalidConfiguration,
        RetryClass::Never,
        "the bundled OpenAI model table is malformed",
    )
}

fn transport_error() -> OpenAiError {
    list_error("request failed")
}

/// Non-secret 16-hex-digit fingerprint of the key the route would send.
///
/// # Errors
/// Returns an error when the credential cannot be resolved.
pub fn openai_api_key_fingerprint(
    reference: &CredentialReference,
    source: &dyn PlatformCredentialSource,
) -> Result<String, OpenAiError> {
    let origin = EndpointOrigin::parse(OPENAI_PLATFORM_ORIGIN)
        .map_err(|_| list_error("origin is invalid"))?;
    let grant = source
        .generation_grant(reference, &origin)
        .map_err(|_| list_error("credential is unavailable"))?;
    Ok(fingerprint_hex(grant.fingerprint().as_bytes()))
}

fn fingerprint_hex(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .take(8)
        .flat_map(|byte| {
            [
                HEX[usize::from(byte >> 4)] as char,
                HEX[usize::from(byte & 0x0f)] as char,
            ]
        })
        .collect()
}

/// Lists every model id the API key can see with one bounded authenticated
/// `GET /v1/models` on the exact Platform origin. No inference is dispatched.
/// The dial origin is not a parameter: a caller cannot point this at another host.
///
/// # Errors
/// Returns the credential, transport, status or parse failure; the caller
/// keeps its cache or the bundled table.
pub async fn list_openai_api_models(
    reference: &CredentialReference,
    source: &dyn PlatformCredentialSource,
    observer: Option<std::sync::Arc<dyn plakous_http::ExchangeObserver>>,
) -> Result<Vec<OpenAiListedModel>, OpenAiError> {
    let origin =
        Origin::parse(OPENAI_PLATFORM_ORIGIN).map_err(|_| list_error("origin is invalid"))?;
    let client = Client::new_http1(plakous_http::Limits::default())
        .map_err(|_| transport_error())?
        .with_observer(observer);
    let key = platform_key(reference, source)?;
    let body = get_models(&client, origin, key.expose(), deadline_instant()?).await?;
    parse_openai_models_page(&body)
}

/// The real listing over a test transport (loopback dial and trust root).
#[cfg(test)]
async fn list_via(
    reference: &CredentialReference,
    source: &dyn PlatformCredentialSource,
    client: &Client,
    dial_origin: Origin,
) -> Result<Vec<OpenAiListedModel>, OpenAiError> {
    let key = platform_key(reference, source)?;
    let body = get_models(client, dial_origin, key.expose(), deadline_instant()?).await?;
    parse_openai_models_page(&body)
}

fn platform_key(
    reference: &CredentialReference,
    source: &dyn PlatformCredentialSource,
) -> Result<crate::SecretString, OpenAiError> {
    let endpoint = EndpointOrigin::parse(OPENAI_PLATFORM_ORIGIN)
        .map_err(|_| list_error("origin is invalid"))?;
    let grant = source
        .generation_grant(reference, &endpoint)
        .map_err(|_| list_error("credential is unavailable"))?;
    source
        .api_key_for_generation(reference, &endpoint, grant)
        .map_err(|_| list_error("credential is unavailable"))
}

async fn get_models(
    client: &Client,
    origin: Origin,
    key: &str,
    deadline: Instant,
) -> Result<Vec<u8>, OpenAiError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("accept"),
        HeaderValue::from_static("application/json"),
    );
    let mut bearer = b"Bearer ".to_vec();
    bearer.extend_from_slice(key.as_bytes());
    headers.insert(
        HeaderName::from_static("authorization"),
        sensitive_header_value(bearer)
            .map_err(|_| list_error("credential is not a header value"))?,
    );
    let request = Request::new(
        origin,
        Method::GET,
        OPENAI_MODELS_PATH,
        headers,
        SensitiveBytes::new(Vec::new()).into_bytes(),
        client.limits(),
    )
    .map_err(|_| transport_error())?;
    let reply = plakous_exchange::lookup::get(
        client,
        request,
        plakous_exchange::lookup::LookupBudget::from_deadline(deadline),
        MAX_BODY_BYTES,
    )
    .await
    .map_err(|error| match error {
        plakous_exchange::lookup::LookupError::BodyLimit => list_error("exceeds the body bound"),
        _ => list_error(&error.to_string()),
    })?;
    if reply.status != StatusCode::OK {
        return Err(list_error(&format!(
            "answered HTTP {}",
            reply.status.as_u16()
        )));
    }
    Ok(reply.body.to_vec())
}

fn deadline_instant() -> Result<Instant, OpenAiError> {
    Instant::now()
        .checked_add(LIST_TIMEOUT)
        .ok_or_else(|| list_error("deadline is invalid"))
}

/// Hosted-feature names a reviewed row may record. Nothing reads them.
const HOSTED_FEATURES: &[&str] = &["web_search", "code_execution", "tool_search"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledTable {
    #[allow(dead_code, reason = "review metadata the census script rewrites")]
    revision: String,
    #[allow(dead_code, reason = "review metadata the census script rewrites")]
    reviewed_at: String,
    #[allow(dead_code, reason = "review metadata the census script rewrites")]
    sources: Vec<String>,
    models: Vec<BundledRow>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledRow {
    id: String,
    max_input_tokens: u64,
    max_tokens: u32,
    efforts: Vec<String>,
    price_threshold_input_tokens: Option<u64>,
    codex_context_window: Option<u64>,
    responses: bool,
    hosted_features: Vec<String>,
}

fn bundled_table() -> Result<&'static BundledTable, OpenAiError> {
    static TABLE: std::sync::OnceLock<Option<BundledTable>> = std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| parse_table(BUNDLED_TABLE))
        .as_ref()
        .ok_or_else(table_error)
}

/// Parses a reviewed table. An effort or hosted-feature name the table does
/// not know fails it rather than being dropped.
fn parse_table(text: &str) -> Option<BundledTable> {
    let table: BundledTable = serde_json::from_str(text).ok()?;
    let known = table.models.iter().all(|row| {
        row.efforts
            .iter()
            .all(|name| EFFORT_LADDER.iter().any(|(known, _)| known == name))
            && row
                .hosted_features
                .iter()
                .all(|name| HOSTED_FEATURES.contains(&name.as_str()))
    });
    known.then_some(table)
}

/// Every id in the bundled reviewed table, in table order.
///
/// # Errors
/// Returns an error when the bundled table is malformed.
pub fn openai_api_bundled_listing() -> Result<Vec<OpenAiListedModel>, OpenAiError> {
    Ok(bundled_table()?
        .models
        .iter()
        .map(|row| OpenAiListedModel { id: row.id.clone() })
        .collect())
}

/// Whether Plak drives one listed model, and why not when it does not.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum OpenAiApiModelStatus {
    /// A reviewed platform profile with reviewed facts.
    Usable,
    /// Listed, but no reviewed profile admits it.
    NeedsReview(String),
    /// Listed, but not a text and tool coding model.
    NotCodingModel(String),
}

/// One listed OpenAI API model with the facts it would run under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiApiModel {
    id: String,
    profile: Option<&'static str>,
    status: OpenAiApiModelStatus,
    efforts: Vec<ReasoningEffort>,
    max_input_tokens: u64,
    max_tokens: u32,
    /// Working-window cap: the documented price threshold, else the Codex
    /// per-model `context_window` when that is below the full input.
    price_threshold_input_tokens: Option<u64>,
}

impl OpenAiApiModel {
    /// Exact model id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Reviewed platform profile id, when a rule admits one.
    #[must_use]
    pub const fn profile(&self) -> Option<&'static str> {
        self.profile
    }
    /// Admission status.
    #[must_use]
    pub const fn status(&self) -> &OpenAiApiModelStatus {
        &self.status
    }
    /// Published effort levels, in ladder order. `none` is explicit.
    #[must_use]
    pub fn efforts(&self) -> &[ReasoningEffort] {
        &self.efforts
    }
    /// Full input window: documented maximum input, else the context window.
    #[must_use]
    pub const fn max_input_tokens(&self) -> u64 {
        self.max_input_tokens
    }
    /// Documented max output. GPT-5.x and GPT-6 pages state 128,000.
    #[must_use]
    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
    }
    /// Working-window cap below the full input, when one is reviewed.
    #[must_use]
    pub const fn price_threshold_input_tokens(&self) -> Option<u64> {
        self.price_threshold_input_tokens
    }

    /// Canonical facts that bind a session.
    #[must_use]
    pub fn facts_digest(&self) -> String {
        let efforts = self
            .efforts
            .iter()
            .map(|effort| effort_name(*effort))
            .collect::<Vec<_>>()
            .join(",");
        plakous_model::sha256_hex(&[
            b"openai.api-model-facts.v2",
            self.id.as_bytes(),
            self.profile.unwrap_or_default().as_bytes(),
            efforts.as_bytes(),
            &self.max_tokens.to_be_bytes(),
        ])
    }
}

fn effort_name(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "none",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
        ReasoningEffort::Max => "max",
        _ => "other",
    }
}

fn efforts_from(names: &[String]) -> Vec<ReasoningEffort> {
    EFFORT_LADDER
        .iter()
        .filter(|(name, _)| names.iter().any(|listed| listed == name))
        .map(|(_, effort)| *effort)
        .collect()
}

/// Classifies every listed id against the bundled table and the family rules,
/// in list order. The live list contributes ids only.
///
/// # Errors
/// Returns an error when the bundled table is malformed.
pub fn openai_api_models(listed: &[OpenAiListedModel]) -> Result<Vec<OpenAiApiModel>, OpenAiError> {
    let table = bundled_table()?;
    Ok(listed
        .iter()
        .map(|row| {
            classify(
                row,
                table.models.iter().find(|candidate| candidate.id == row.id),
            )
        })
        .collect())
}

fn not_coding(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    NOT_CODING_MARKERS.iter().any(|marker| id.contains(marker))
}

fn text_responses(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.starts_with("gpt-")
        || id.starts_with("o1")
        || id.starts_with("o3")
        || id.starts_with("o4")
        || id.starts_with("chatgpt-")
}

fn classify(listed: &OpenAiListedModel, reviewed: Option<&BundledRow>) -> OpenAiApiModel {
    let id = listed.id.as_str();
    if not_coding(id) {
        return row_from(
            id,
            None,
            OpenAiApiModelStatus::NotCodingModel(
                "audio, image, embedding, realtime, moderation, speech or video model".to_owned(),
            ),
            reviewed,
        );
    }
    let profile = REVIEWED_PROFILES
        .iter()
        .find(|profile| **profile == id)
        .copied();
    let efforts = reviewed.map_or(Vec::new(), |row| efforts_from(&row.efforts));
    let max_input = reviewed.map_or(0, |row| row.max_input_tokens);
    let max_tokens = reviewed.map_or(0, |row| row.max_tokens);
    let responses = reviewed.is_some_and(|row| row.responses);
    let status = if profile.is_some() {
        if reviewed.is_none() {
            OpenAiApiModelStatus::NeedsReview(
                "not in this build's reviewed OpenAI model table".to_owned(),
            )
        } else if !responses
            || efforts.is_empty()
            || max_input == 0
            || max_tokens != REVIEWED_OUTPUT_BOUND
        {
            OpenAiApiModelStatus::NeedsReview(
                "its reviewed facts are outside the platform profile".to_owned(),
            )
        } else {
            OpenAiApiModelStatus::Usable
        }
    } else if text_responses(id) {
        OpenAiApiModelStatus::NeedsReview(
            "this Responses model has no reviewed OpenAI platform profile".to_owned(),
        )
    } else if reviewed.is_none() {
        OpenAiApiModelStatus::NeedsReview(
            "not in this build's reviewed OpenAI model table".to_owned(),
        )
    } else {
        OpenAiApiModelStatus::NeedsReview(
            "this model is not reviewed for the OpenAI API".to_owned(),
        )
    };
    row_from(
        id,
        profile.filter(|_| status == OpenAiApiModelStatus::Usable),
        status,
        reviewed,
    )
}

fn row_from(
    id: &str,
    profile: Option<&'static str>,
    status: OpenAiApiModelStatus,
    reviewed: Option<&BundledRow>,
) -> OpenAiApiModel {
    let max_input_tokens = reviewed.map_or(0, |row| row.max_input_tokens);
    let efforts = reviewed.map_or(Vec::new(), |row| efforts_from(&row.efforts));
    let price = reviewed.and_then(|row| row.price_threshold_input_tokens);
    let codex = reviewed.and_then(|row| row.codex_context_window);
    // Price threshold wins. Otherwise the Codex per-model window, when it is
    // below the full input. A Codex window above the documented max input is
    // not a working window.
    let price_threshold_input_tokens = match price {
        Some(threshold) => Some(threshold),
        None => codex.filter(|window| max_input_tokens > 0 && *window < max_input_tokens),
    };
    OpenAiApiModel {
        id: id.to_owned(),
        profile,
        status,
        efforts,
        max_input_tokens,
        max_tokens: reviewed.map_or(0, |row| row.max_tokens),
        price_threshold_input_tokens,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
#[path = "api_models_tests.rs"]
mod tests;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "loopback listing tests assert the transport"
)]
#[path = "api_models_live_tests.rs"]
mod live_tests;
