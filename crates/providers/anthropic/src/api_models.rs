//! Anthropic API-key model catalog: the provider's live `GET /v1/models`
//! list, the bundled reviewed table and the reviewed family rules.
//!
//! The live list attests that a model exists and the facts Anthropic
//! publishes for it (input window, output limit, effort levels, lifecycle). It
//! never mints semantics: whether and how Plak drives a model is the family
//! rule's decision, and pricing thresholds and hosted-feature support come
//! only from the bundled table a reviewer checked in.

use std::time::Instant;

use plakous_http::{
    Client, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request, SensitiveBytes,
    StatusCode, sensitive_header_value,
};
use plakous_model::ReasoningEffort;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ANTHROPIC_API_VERSION, ANTHROPIC_PLATFORM_ORIGIN, AnthropicApiKeySource, AnthropicError,
    AnthropicErrorKind, AnthropicErrorPhase,
};

/// Exact Models API path on the Platform origin.
const ANTHROPIC_MODELS_PATH: &str = "/v1/models";
/// The output limit the reviewed adaptive families are admitted under.
pub(crate) const REVIEWED_ADAPTIVE_OUTPUT_CEILING: u32 = 128_000;

const BUNDLED_TABLE: &str = include_str!("api_models.json");
const PAGE_LIMIT: usize = 1000;
const MAX_PAGES: usize = 8;
const MAX_MODELS: usize = 2048;
const MAX_ID_BYTES: usize = 128;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const LIFECYCLES: [&str; 3] = ["active", "deprecated", "retired"];
const EFFORT_LADDER: [(&str, ReasoningEffort); 5] = [
    ("low", ReasoningEffort::Low),
    ("medium", ReasoningEffort::Medium),
    ("high", ReasoningEffort::High),
    ("xhigh", ReasoningEffort::XHigh),
    ("max", ReasoningEffort::Max),
];

/// One `/v1/models` row, reduced to the facts admission reads.
///
/// Unknown fields are additive upstream growth and are dropped; a row whose
/// read fields are malformed fails the whole page.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AnthropicListedModel {
    /// Exact model id.
    pub id: String,
    /// Published input window.
    pub max_input_tokens: Option<u64>,
    /// Published output limit.
    pub max_tokens: Option<u32>,
    /// Published effort levels Plak knows, in ladder order; `None` when the
    /// row states no effort capability.
    pub efforts: Option<Vec<String>>,
    /// Whether the row states adaptive thinking support.
    pub adaptive_thinking: Option<bool>,
    /// Published lifecycle (`active`, `deprecated`, `retired`, ...).
    pub lifecycle: Option<String>,
    /// Published deprecation time, when any.
    pub deprecated_at: Option<String>,
    /// Published retirement time, when any.
    pub retires_at: Option<String>,
}

/// One parsed `/v1/models` page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AnthropicModelsPage {
    pub(crate) models: Vec<AnthropicListedModel>,
    pub(crate) has_more: bool,
    pub(crate) last_id: Option<String>,
}

/// Parses one `/v1/models` response body.
///
/// # Errors
/// Returns a protocol error for malformed JSON, missing or unsafe ids,
/// duplicate ids, or malformed fields admission reads.
pub(crate) fn parse_anthropic_models_page(
    body: &[u8],
) -> Result<AnthropicModelsPage, AnthropicError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| list_error("is not JSON"))?;
    let rows = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| list_error("has no data array"))?;
    if rows.len() > MAX_MODELS {
        return Err(list_error("exceeds the row bound"));
    }
    let models = rows.iter().map(listed_row).collect::<Result<Vec<_>, _>>()?;
    let models = accept_anthropic_listed_models(&models)?;
    let has_more = match value.get("has_more") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(more)) => *more,
        Some(_) => return Err(list_error("has a malformed has_more")),
    };
    let last_id = optional_string(&value, "last_id")?;
    Ok(AnthropicModelsPage {
        models,
        has_more,
        last_id,
    })
}

fn listed_row(row: &Value) -> Result<AnthropicListedModel, AnthropicError> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| safe_id(id))
        .ok_or_else(|| list_error("has a missing or unsafe model id"))?
        .to_owned();
    let max_input_tokens = optional_u64(row, "max_input_tokens")?;
    let max_tokens = optional_u64(row, "max_tokens")?
        .map(|value| u32::try_from(value).map_err(|_| list_error("has an oversized max_tokens")))
        .transpose()?;
    let capabilities = row.get("capabilities");
    let supported = |value: Option<&Value>| {
        value
            .and_then(|value| value.get("supported"))
            .and_then(Value::as_bool)
    };
    let effort = capabilities.and_then(|caps| caps.get("effort"));
    let efforts = match supported(effort) {
        None => None,
        Some(false) => Some(Vec::new()),
        Some(true) => Some(
            EFFORT_LADDER
                .iter()
                .filter(|(name, _)| supported(effort.and_then(|e| e.get(*name))) == Some(true))
                .map(|(name, _)| (*name).to_owned())
                .collect(),
        ),
    };
    let adaptive_thinking = supported(
        capabilities
            .and_then(|caps| caps.get("thinking"))
            .and_then(|thinking| thinking.get("types"))
            .and_then(|types| types.get("adaptive")),
    );
    Ok(AnthropicListedModel {
        id,
        max_input_tokens,
        max_tokens,
        efforts,
        adaptive_thinking,
        lifecycle: optional_string(row, "lifecycle")?,
        deprecated_at: optional_string(row, "deprecated_at")?,
        retires_at: optional_string(row, "retires_at")?,
    })
}

/// Validates listed rows the way a fetched page is validated: the row bound,
/// safe and unique ids, efforts on the ladder without repeats, bounded
/// strings and RFC 3339 lifecycle times. A cached list takes the same checks.
///
/// # Errors
/// Returns a protocol error naming the first failed check; the whole list
/// fails.
pub fn accept_anthropic_listed_models(
    listed: &[AnthropicListedModel],
) -> Result<Vec<AnthropicListedModel>, AnthropicError> {
    if listed.len() > MAX_MODELS {
        return Err(list_error("exceeds the row bound"));
    }
    let mut ids = std::collections::BTreeSet::new();
    for row in listed {
        if !safe_id(&row.id) {
            return Err(list_error("has a missing or unsafe model id"));
        }
        if !ids.insert(row.id.as_str()) {
            return Err(list_error("repeats a model id"));
        }
        if row.max_input_tokens == Some(0) || row.max_tokens == Some(0) {
            return Err(list_error("has a malformed token limit"));
        }
        if let Some(efforts) = &row.efforts {
            let mut seen = std::collections::BTreeSet::new();
            if efforts.iter().any(|name| {
                !EFFORT_LADDER.iter().any(|(known, _)| known == name) || !seen.insert(name)
            }) {
                return Err(list_error("has a malformed effort list"));
            }
        }
        for text in [&row.lifecycle, &row.deprecated_at, &row.retires_at]
            .into_iter()
            .flatten()
        {
            if text.len() > MAX_ID_BYTES {
                return Err(list_error("has a malformed string field"));
            }
        }
        for time in [&row.deprecated_at, &row.retires_at].into_iter().flatten() {
            if unix_seconds(time).is_none() {
                return Err(list_error("has a malformed lifecycle time"));
            }
        }
    }
    Ok(listed.to_vec())
}

fn optional_u64(row: &Value, key: &str) -> Result<Option<u64>, AnthropicError> {
    match row.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| list_error("has a malformed token limit")),
    }
}

fn optional_string(row: &Value, key: &str) -> Result<Option<String>, AnthropicError> {
    match row.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.len() <= MAX_ID_BYTES => Ok(Some(value.clone())),
        Some(_) => Err(list_error("has a malformed string field")),
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

fn transport_error<E>(_: E) -> AnthropicError {
    list_error("request failed")
}

fn list_error(reason: &str) -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidResponse,
        AnthropicErrorPhase::Prepare,
        format!("Anthropic model list {reason}"),
    )
}

/// Lists every model the API key can see with bounded authenticated
/// `GET /v1/models` requests on the exact Platform origin, following
/// pagination. No inference is dispatched. The dial origin is not a
/// parameter; an installed exchange observer sees the requests (their bodies
/// are empty and the key header is sensitive).
///
/// # Errors
/// Returns the credential, transport, status or parse failure; the caller
/// keeps its cache or the bundled table.
pub async fn list_anthropic_api_models(
    source: &dyn AnthropicApiKeySource,
    observer: Option<std::sync::Arc<dyn plakous_http::ExchangeObserver>>,
) -> Result<Vec<AnthropicListedModel>, AnthropicError> {
    let origin =
        Origin::parse(ANTHROPIC_PLATFORM_ORIGIN).map_err(|_| list_error("origin is invalid"))?;
    let client = Client::new_http1(plakous_http::Limits::default())
        .map_err(transport_error)?
        .with_observer(observer);
    list_from(source, &client, origin, list_deadline()?).await
}

fn list_deadline() -> Result<Instant, AnthropicError> {
    Instant::now()
        .checked_add(LIST_TIMEOUT)
        .ok_or_else(|| list_error("deadline is invalid"))
}

async fn list_from(
    source: &dyn AnthropicApiKeySource,
    client: &Client,
    origin: Origin,
    deadline: Instant,
) -> Result<Vec<AnthropicListedModel>, AnthropicError> {
    let deadline = plakous_exchange::lookup::LookupBudget::from_deadline(deadline);
    let key = source.current_key()?;
    let mut models: Vec<AnthropicListedModel> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let path = match &after {
            None => format!("{ANTHROPIC_MODELS_PATH}?limit={PAGE_LIMIT}"),
            Some(cursor) => format!("{ANTHROPIC_MODELS_PATH}?limit={PAGE_LIMIT}&after_id={cursor}"),
        };
        let body = get_page(client, origin.clone(), &path, key.expose(), deadline).await?;
        let page = parse_anthropic_models_page(&body)?;
        let last_row = page.models.last().map(|row| row.id.clone());
        for model in page.models {
            if models.iter().any(|seen| seen.id == model.id) {
                return Err(list_error("repeats a model id across pages"));
            }
            if models.len() >= MAX_MODELS {
                return Err(list_error("exceeds the row bound"));
            }
            models.push(model);
        }
        match (page.has_more, page.last_id) {
            (true, Some(cursor)) if safe_id(&cursor) && Some(&cursor) == last_row.as_ref() => {
                after = Some(cursor);
            }
            (true, _) => return Err(list_error("has more pages without a safe cursor")),
            (false, _) => return Ok(models),
        }
    }
    Err(list_error("exceeds the page bound"))
}

async fn get_page(
    client: &Client,
    origin: Origin,
    path: &str,
    key: &str,
    deadline: plakous_exchange::lookup::LookupBudget,
) -> Result<Vec<u8>, AnthropicError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("accept"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static(ANTHROPIC_API_VERSION),
    );
    headers.insert(
        HeaderName::from_static("x-api-key"),
        sensitive_header_value(key.as_bytes().to_vec())
            .map_err(|_| list_error("credential is not a header value"))?,
    );
    let request = Request::new(
        origin,
        Method::GET,
        path,
        headers,
        SensitiveBytes::new(Vec::new()).into_bytes(),
        client.limits(),
    )
    .map_err(transport_error)?;
    let reply = plakous_exchange::lookup::get(client, request, deadline, MAX_BODY_BYTES)
        .await
        .map_err(|error| match error {
            plakous_exchange::lookup::LookupError::BodyLimit => {
                list_error("exceeds the body bound")
            }
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

/// Hosted features the reviewed table records per model, from the docs'
/// `featureMetadata.supportedModels` (and the Models API for web search).
/// Recorded for later use; no request reads them and no session binds them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one independent documented flag per hosted feature"
)]
pub(crate) struct AnthropicHostedFeatures {
    /// Provider-side compaction (`compact_20260112`).
    pub(crate) compaction: bool,
    /// Hosted programmatic tool calling.
    pub(crate) hosted_ptc: bool,
    /// Server tool search.
    pub(crate) tool_search: bool,
    /// Server web search.
    pub(crate) web_search: bool,
    /// Server web fetch.
    pub(crate) web_fetch: bool,
    /// Server code execution.
    pub(crate) code_execution: bool,
}

impl AnthropicHostedFeatures {
    fn from_names(names: &[String]) -> Result<Self, AnthropicError> {
        let mut features = Self::default();
        for name in names {
            match name.as_str() {
                "compaction" => features.compaction = true,
                "hosted_ptc" => features.hosted_ptc = true,
                "tool_search" => features.tool_search = true,
                "web_search" => features.web_search = true,
                "web_fetch" => features.web_fetch = true,
                "code_execution" => features.code_execution = true,
                _ => return Err(table_error()),
            }
        }
        Ok(features)
    }
}

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
    adaptive_thinking: bool,
    price_threshold_input_tokens: Option<u64>,
    lifecycle: String,
    hosted_features: Vec<String>,
}

fn table_error() -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidConfiguration,
        AnthropicErrorPhase::Prepare,
        "the bundled Anthropic model table is malformed",
    )
}

fn bundled_table() -> Result<&'static BundledTable, AnthropicError> {
    static TABLE: std::sync::OnceLock<Option<BundledTable>> = std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| serde_json::from_str(BUNDLED_TABLE).ok())
        .as_ref()
        .ok_or_else(table_error)
}

/// Every id in the bundled reviewed table, in table order: the list the
/// product falls back to when the live list and its cache are unavailable.
///
/// # Errors
/// Returns an error when the bundled table is malformed.
pub fn anthropic_api_bundled_listing() -> Result<Vec<AnthropicListedModel>, AnthropicError> {
    Ok(bundled_table()?
        .models
        .iter()
        .map(|row| AnthropicListedModel {
            id: row.id.clone(),
            max_input_tokens: Some(row.max_input_tokens),
            max_tokens: Some(row.max_tokens),
            efforts: Some(row.efforts.clone()),
            adaptive_thinking: Some(row.adaptive_thinking),
            lifecycle: Some(row.lifecycle.clone()),
            deprecated_at: None,
            retires_at: None,
        })
        .collect())
}

/// What the reviewed family rule decides for one id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FamilyRule {
    /// Adaptive thinking and local client tools under the stable API profile.
    Adaptive { family: &'static str },
    /// The family exists but this route has not reviewed how to drive it.
    NotReviewed(&'static str),
}

const LINES: [(&str, &str, &str); 4] = [
    ("opus", "claude-5-opus", "claude-4-opus"),
    ("sonnet", "claude-5-sonnet", "claude-4-sonnet"),
    ("haiku", "claude-5-haiku", "claude-4-haiku"),
    ("fable", "claude-5-fable", "claude-4-fable"),
];

/// The reviewed family rules. Claude 5.x and Claude 4.6 and later think
/// adaptively and run under the stable adaptive client-tools profile; Claude
/// 4.5 takes manual thinking budgets, which this route has not reviewed.
fn family_rule(id: &str) -> FamilyRule {
    let Some(rest) = id.strip_prefix("claude-") else {
        return FamilyRule::NotReviewed("not a Claude model id");
    };
    let mut parts = rest.split('-');
    let line = parts.next().unwrap_or_default();
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = match parts.next() {
        None => Some(0),
        Some(part) if part.len() <= 2 => part.parse::<u32>().ok(),
        // A dated snapshot without a minor version.
        Some(_) => Some(0),
    };
    let Some(&(_, five, four)) = LINES.iter().find(|(name, ..)| *name == line) else {
        return FamilyRule::NotReviewed("this model line is not reviewed for the Anthropic API");
    };
    match (major, minor) {
        (Some(5..), Some(_)) => FamilyRule::Adaptive { family: five },
        (Some(4), Some(6..)) if matches!(line, "opus" | "sonnet") => {
            FamilyRule::Adaptive { family: four }
        }
        (Some(4), Some(5)) => FamilyRule::NotReviewed(
            "Claude 4.5 takes manual thinking budgets, which the Anthropic API route has not reviewed",
        ),
        _ => FamilyRule::NotReviewed("this model family is not reviewed for the Anthropic API"),
    }
}

/// Whether Plak drives one listed model, and why not when it does not.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AnthropicApiModelStatus {
    /// A reviewed family with reviewed facts: admitted.
    Usable,
    /// Listed, but no reviewed family rule or table row admits it.
    NeedsReview(String),
    /// Listed, but not a text and tool coding model.
    NotCodingModel(String),
    /// Retired per its lifecycle fields.
    Retired(String),
}

/// One listed Anthropic API model with the facts it would run under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicApiModel {
    id: String,
    family: Option<&'static str>,
    status: AnthropicApiModelStatus,
    efforts: Vec<ReasoningEffort>,
    max_input_tokens: u64,
    max_tokens: u32,
    price_threshold_input_tokens: Option<u64>,
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "recorded for later use; only tests read it")
    )]
    hosted_features: AnthropicHostedFeatures,
    deprecation: Option<String>,
}

impl AnthropicApiModel {
    /// Exact model id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Reviewed family, when a rule admits one.
    #[must_use]
    pub const fn family(&self) -> Option<&'static str> {
        self.family
    }
    /// Admission status.
    #[must_use]
    pub const fn status(&self) -> &AnthropicApiModelStatus {
        &self.status
    }
    /// The model's published effort levels, in ladder order.
    #[must_use]
    pub fn efforts(&self) -> &[ReasoningEffort] {
        &self.efforts
    }
    /// Full input window.
    #[must_use]
    pub const fn max_input_tokens(&self) -> u64 {
        self.max_input_tokens
    }
    /// The model's own output limit.
    #[must_use]
    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
    }
    /// Prompt size above which Anthropic prices input higher, when the
    /// pricing docs publish one for this model.
    #[must_use]
    pub const fn price_threshold_input_tokens(&self) -> Option<u64> {
        self.price_threshold_input_tokens
    }
    /// Recorded hosted-feature support.
    #[cfg(test)]
    pub(crate) const fn hosted_features(&self) -> AnthropicHostedFeatures {
        self.hosted_features
    }
    /// A deprecation notice from the lifecycle fields, when any.
    #[must_use]
    pub fn deprecation(&self) -> Option<&str> {
        self.deprecation.as_deref()
    }

    /// Canonical facts that bind a session: what it would send (model,
    /// family, effort ladder, output limit). The input window a session runs
    /// under is bound separately as the window it chose; pricing thresholds
    /// and hosted-feature records change nothing a session sends.
    #[must_use]
    pub fn facts_digest(&self) -> String {
        let efforts = self
            .efforts
            .iter()
            .map(|effort| effort_name(*effort))
            .collect::<Vec<_>>()
            .join(",");
        plakous_model::sha256_hex(&[
            b"anthropic.api-model-facts.v2",
            self.id.as_bytes(),
            self.family.unwrap_or_default().as_bytes(),
            efforts.as_bytes(),
            &self.max_tokens.to_be_bytes(),
        ])
    }

    /// The reviewed bundled facts for one id, without a live list,
    /// classified at the current time; an offline and test seam.
    ///
    /// # Errors
    /// Returns an invalid-configuration error when the id is not a usable
    /// bundled row.
    pub fn bundled(id: &str) -> Result<Self, AnthropicError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let model = anthropic_api_models(&anthropic_api_bundled_listing()?, now)?
            .into_iter()
            .find(|model| model.id == id)
            .ok_or_else(|| unusable(id))?;
        if model.status == AnthropicApiModelStatus::Usable {
            Ok(model)
        } else {
            Err(unusable(id))
        }
    }
}

fn unusable(id: &str) -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidConfiguration,
        AnthropicErrorPhase::Prepare,
        format!("Anthropic API model `{id}` is not admitted"),
    )
}

pub(crate) const fn effort_name(effort: ReasoningEffort) -> &'static str {
    match effort {
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

/// Classifies every listed model against the bundled table and the family
/// rules, in list order. A live fact may lower the reviewed one (a smaller
/// window or output limit, fewer efforts) but never raise it: a live window,
/// output limit, effort or adaptive-thinking claim above the reviewed row, or
/// a lifecycle other than `active`, `deprecated` or `retired`, is
/// `NeedsReview`. Pricing thresholds and hosted features come only from the
/// table.
///
/// `now_unix_seconds` decides whether a published retirement time has passed.
///
/// # Errors
/// Returns an error when the listed rows fail the page checks of
/// [`accept_anthropic_listed_models`] or the bundled table is malformed.
pub fn anthropic_api_models(
    listed: &[AnthropicListedModel],
    now_unix_seconds: u64,
) -> Result<Vec<AnthropicApiModel>, AnthropicError> {
    let table = bundled_table()?;
    accept_anthropic_listed_models(listed)?
        .iter()
        .map(|row| {
            classify(
                row,
                table.models.iter().find(|r| r.id == row.id),
                now_unix_seconds,
            )
        })
        .collect()
}

fn classify(
    listed: &AnthropicListedModel,
    reviewed: Option<&BundledRow>,
    now: u64,
) -> Result<AnthropicApiModel, AnthropicError> {
    let rule = family_rule(&listed.id);
    let efforts = listed
        .efforts
        .as_deref()
        .or(reviewed.map(|row| row.efforts.as_slice()))
        .map(efforts_from)
        .unwrap_or_default();
    let max_input_tokens = listed
        .max_input_tokens
        .or(reviewed.map(|row| row.max_input_tokens))
        .unwrap_or_default();
    let max_tokens = listed
        .max_tokens
        .or(reviewed.map(|row| row.max_tokens))
        .unwrap_or_default();
    let adaptive = listed
        .adaptive_thinking
        .or(reviewed.map(|row| row.adaptive_thinking))
        .unwrap_or(false);
    let lifecycle = listed
        .lifecycle
        .clone()
        .or(reviewed.map(|row| row.lifecycle.clone()));
    let retired = lifecycle.as_deref() == Some("retired")
        || listed
            .retires_at
            .as_deref()
            .and_then(unix_seconds)
            .is_some_and(|at| at <= now);
    let deprecation = (lifecycle.as_deref() == Some("deprecated")
        || listed.deprecated_at.is_some())
    .then(|| match &listed.retires_at {
        Some(at) => format!("deprecated by Anthropic; retires {at}"),
        None => "deprecated by Anthropic".to_owned(),
    });
    let unknown_lifecycle = lifecycle
        .as_deref()
        .is_some_and(|value| !LIFECYCLES.contains(&value));
    let live_raises = reviewed.is_some_and(|row| live_exceeds(listed, row));
    let status = if unknown_lifecycle {
        AnthropicApiModelStatus::NeedsReview(
            "Anthropic lists an unknown lifecycle for it".to_owned(),
        )
    } else if retired {
        AnthropicApiModelStatus::Retired(format!(
            "retired by Anthropic{}",
            listed
                .retires_at
                .as_deref()
                .map_or_else(String::new, |at| format!(" ({at})"))
        ))
    } else {
        match (rule, reviewed) {
            (FamilyRule::NotReviewed(reason), _) => {
                AnthropicApiModelStatus::NeedsReview(reason.to_owned())
            }
            (FamilyRule::Adaptive { .. }, None) => AnthropicApiModelStatus::NeedsReview(
                "not in this build's reviewed Anthropic model table".to_owned(),
            ),
            (FamilyRule::Adaptive { .. }, Some(_)) if live_raises => {
                AnthropicApiModelStatus::NeedsReview(
                    "the live list publishes a higher window, output limit or effort than the reviewed table"
                        .to_owned(),
                )
            }
            (FamilyRule::Adaptive { .. }, Some(_)) if !adaptive => {
                AnthropicApiModelStatus::NeedsReview(
                    "Anthropic does not list adaptive thinking for it".to_owned(),
                )
            }
            (FamilyRule::Adaptive { .. }, Some(_)) if efforts.is_empty() => {
                AnthropicApiModelStatus::NeedsReview(
                    "Anthropic lists no effort levels for it".to_owned(),
                )
            }
            (FamilyRule::Adaptive { .. }, Some(_))
                if max_tokens == 0
                    || max_tokens > REVIEWED_ADAPTIVE_OUTPUT_CEILING
                    || max_input_tokens == 0 =>
            {
                AnthropicApiModelStatus::NeedsReview(
                    "its published token limits are outside the reviewed family's".to_owned(),
                )
            }
            (FamilyRule::Adaptive { .. }, Some(_)) => AnthropicApiModelStatus::Usable,
        }
    };
    let family = match rule {
        FamilyRule::Adaptive { family } => Some(family),
        FamilyRule::NotReviewed(_) => None,
    };
    Ok(AnthropicApiModel {
        id: listed.id.clone(),
        family,
        status,
        efforts,
        max_input_tokens,
        max_tokens,
        price_threshold_input_tokens: reviewed.and_then(|row| row.price_threshold_input_tokens),
        hosted_features: reviewed
            .map(|row| AnthropicHostedFeatures::from_names(&row.hosted_features))
            .transpose()?
            .unwrap_or_default(),
        deprecation,
    })
}

fn live_exceeds(listed: &AnthropicListedModel, reviewed: &BundledRow) -> bool {
    listed
        .max_input_tokens
        .is_some_and(|live| live > reviewed.max_input_tokens)
        || listed
            .max_tokens
            .is_some_and(|live| live > reviewed.max_tokens)
        || listed
            .efforts
            .iter()
            .flatten()
            .any(|name| !reviewed.efforts.contains(name))
        || (listed.adaptive_thinking == Some(true) && !reviewed.adaptive_thinking)
}

fn unix_seconds(text: &str) -> Option<u64> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()
        .and_then(|at| u64::try_from(at.unix_timestamp()).ok())
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
