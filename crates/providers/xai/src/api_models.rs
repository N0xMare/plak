//! xAI API-key model catalog: the live `GET /v1/models` list, the bundled
//! reviewed table and the family rules.
//!
//! The live row may carry `context_length`, `long_context_threshold` and
//! `capabilities.reasoning_effort`. Those numbers only narrow a reviewed row:
//! a live window, threshold or effort above the table makes the id
//! `NeedsReview`, and a lower one is adopted (the smaller window or
//! threshold, the live efforts that are also reviewed).
//!
//! xAI publishes no output limit; input and output share `context_length`.
//! As on a route with no attested output limit, the output bound is reserved
//! out of that window: the platform's create-response ceiling
//! ([`crate::XAI_PLATFORM_MAX_OUTPUT_TOKENS`]), capped at half the context.
//! The full input window is `context_length` less that reserve, and the
//! working window is the long-context price threshold (200,000 on the
//! reviewed rows) where it is smaller, so either window plus the output bound
//! fits in `context_length`.
//!
//! Usable ids are the grok-4.x reasoning models the existing platform profile
//! already drives (`grok-4.3`, `grok-4.5`, `grok-4.6`, `grok-4.7`). A
//! non-reasoning or multi-agent variant, and `grok-build-0.1` (no effort
//! parameter), need review. Image and video ids are not coding models.

use std::time::Instant;

use plakous_http::{
    Client, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request, SensitiveBytes,
    StatusCode, sensitive_header_value,
};
use plakous_model::ReasoningEffort;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    XaiPlatformApiKeySource, XaiPlatformBuildError, platform::XAI_PLATFORM_API_KEY_ORIGIN,
};

/// Exact Models API path on the Platform origin.
const XAI_MODELS_PATH: &str = "/v1/models";

const BUNDLED_TABLE: &str = include_str!("api_models.json");
const MAX_MODELS: usize = 2048;
const MAX_ID_BYTES: usize = 128;
pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const EFFORT_LADDER: &[(&str, ReasoningEffort)] = &[
    ("none", ReasoningEffort::None),
    ("low", ReasoningEffort::Low),
    ("medium", ReasoningEffort::Medium),
    ("high", ReasoningEffort::High),
    ("xhigh", ReasoningEffort::XHigh),
];
/// grok-4.x reasoning models the existing platform profile drives.
const USABLE_PROFILES: &[&str] = &["grok-4.3", "grok-4.5", "grok-4.6", "grok-4.7"];
const NOT_CODING_MARKERS: &[&str] = &["imagine", "image", "video", "tts", "embed", "audio", "flux"];

/// One `/v1/models` row the catalog classifies. Prices are not retained.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct XaiListedModel {
    /// Exact model id.
    pub id: String,
    /// Provider `created` unix seconds. A missing or non-integer value fails
    /// the listing rather than being skipped.
    pub created: u64,
    /// Published `context_length`, when the row carries one.
    pub context_length: Option<u64>,
    /// Published long-context price threshold, when the row carries one.
    pub long_context_threshold: Option<u64>,
    /// Published reasoning-effort names, in payload order.
    pub efforts: Vec<String>,
}

/// Whether Plak drives one listed model, and why not when it does not.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum XaiApiModelStatus {
    /// A reviewed grok-4.x reasoning model under the platform profile.
    Usable,
    /// Listed, but no reviewed rule admits it.
    NeedsReview(String),
    /// Listed, but not a text coding model.
    NotCodingModel(String),
}

/// One listed xAI API model with the facts it would run under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XaiApiModel {
    id: String,
    status: XaiApiModelStatus,
    efforts: Vec<ReasoningEffort>,
    context_length: u64,
    /// Working-window cap: the long-context price threshold when reviewed.
    price_threshold_input_tokens: Option<u64>,
    max_tokens: u32,
}

impl XaiApiModel {
    /// Exact model id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Admission status.
    #[must_use]
    pub const fn status(&self) -> &XaiApiModelStatus {
        &self.status
    }
    /// Published effort levels, in ladder order.
    #[must_use]
    pub fn efforts(&self) -> &[ReasoningEffort] {
        &self.efforts
    }
    /// Published context (`context_length`), shared by input and output.
    #[must_use]
    pub const fn context_length(&self) -> u64 {
        self.context_length
    }
    /// Full input window: the context less the reserved output bound.
    #[must_use]
    pub fn max_input_tokens(&self) -> u64 {
        self.context_length
            .saturating_sub(u64::from(self.max_tokens))
    }
    /// Long-context price threshold, when one is reviewed.
    #[must_use]
    pub const fn price_threshold_input_tokens(&self) -> Option<u64> {
        self.price_threshold_input_tokens
    }
    /// The working window: the price threshold where it is below the full
    /// input window, else the full input window.
    #[must_use]
    pub fn working_window(&self) -> u64 {
        let full = self.max_input_tokens();
        match self.price_threshold_input_tokens {
            Some(threshold) if threshold < full => threshold,
            _ => full,
        }
    }
    /// Output bound reserved out of the context. xAI publishes none.
    #[must_use]
    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
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
            b"xai.api-model-facts.v2",
            self.id.as_bytes(),
            efforts.as_bytes(),
            &self.max_tokens.to_be_bytes(),
        ])
    }
}

/// Whether the bundled table admits `effort` on a usable `model`.
pub(crate) fn xai_platform_effort_admitted(model: &str, effort: ReasoningEffort) -> bool {
    usable_row(model).is_some_and(|row| efforts_from(&row.efforts).contains(&effort))
}

/// Whether `input` plus `output` tokens fit a usable `model`'s reviewed
/// context with `output` at most the reserved output bound.
pub(crate) fn xai_platform_capacity_admitted(model: &str, input: u64, output: u32) -> bool {
    usable_row(model).is_some_and(|row| {
        output <= output_reserve(row.context_length)
            && input
                .checked_add(u64::from(output))
                .is_some_and(|total| total <= row.context_length)
    })
}

fn usable_row(model: &str) -> Option<&'static BundledRow> {
    if !USABLE_PROFILES.contains(&model) {
        return None;
    }
    bundled_table()
        .ok()?
        .models
        .iter()
        .find(|row| row.id == model)
}

/// The output bound reserved out of a shared context: the platform's
/// create-response ceiling, capped at half the context.
fn output_reserve(context_length: u64) -> u32 {
    u32::try_from((context_length / 2).min(u64::from(crate::XAI_PLATFORM_MAX_OUTPUT_TOKENS)))
        .unwrap_or(0)
}

/// The bundled table as a listing, for offline admission.
///
/// # Errors
/// Returns an error when the bundled table is malformed.
pub fn xai_api_bundled_listing() -> Result<Vec<XaiListedModel>, XaiApiModelError> {
    Ok(bundled_table()?
        .models
        .iter()
        .map(|row| XaiListedModel {
            id: row.id.clone(),
            created: 0,
            context_length: Some(row.context_length),
            long_context_threshold: row.long_context_threshold,
            efforts: row.efforts.clone(),
        })
        .collect())
}

/// Classifies every listed id. Live numbers that exceed the reviewed row
/// mark the id `NeedsReview` and are not adopted; lower ones narrow it.
///
/// # Errors
/// Returns an error when the bundled table is malformed.
pub fn xai_api_models(listed: &[XaiListedModel]) -> Result<Vec<XaiApiModel>, XaiApiModelError> {
    let table = bundled_table()?;
    listed
        .iter()
        .map(|row| {
            classify(
                row,
                table.models.iter().find(|candidate| candidate.id == row.id),
            )
        })
        .collect()
}

/// The same id checks a fetched page uses. Cached rows go through this before
/// classification.
///
/// # Errors
/// Returns an error for a duplicate id or a list over the row bound.
pub fn accept_xai_listed_models(
    models: &[XaiListedModel],
) -> Result<Vec<XaiListedModel>, XaiApiModelError> {
    if models.len() > MAX_MODELS {
        return Err(list_error("exceeds the row bound"));
    }
    let mut accepted = Vec::with_capacity(models.len());
    for model in models {
        if !safe_id(&model.id) {
            continue;
        }
        if accepted
            .iter()
            .any(|seen: &XaiListedModel| seen.id == model.id)
        {
            return Err(list_error("repeats a model id"));
        }
        accepted.push(model.clone());
    }
    Ok(accepted)
}

/// Parses one `/v1/models` page. A cursor, `has_more`, an unknown lifecycle
/// or an unparseable timestamp fails the page.
///
/// # Errors
/// Returns an error when the page is not a closed, bounded model list.
pub(crate) fn parse_xai_models_page(body: &[u8]) -> Result<Vec<XaiListedModel>, XaiApiModelError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| list_error("is not JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| list_error("is not an object"))?;
    if object.get("has_more").and_then(Value::as_bool) == Some(true) {
        return Err(list_error("has another page"));
    }
    for key in ["next_cursor", "next_page", "after"] {
        if object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|cursor| !cursor.is_empty())
        {
            return Err(list_error("has another page"));
        }
    }
    let data = match (object.get("data"), object.get("models")) {
        (Some(data), None) => data,
        (None, Some(models)) => models,
        _ => return Err(list_error("has no single model array")),
    };
    let rows = data
        .as_array()
        .ok_or_else(|| list_error("has no single model array"))?;
    if rows.len() > MAX_MODELS {
        return Err(list_error("exceeds the row bound"));
    }
    let mut listed = Vec::with_capacity(rows.len());
    for row in rows {
        listed.push(parse_row(row)?);
    }
    accept_xai_listed_models(&listed)
}

fn parse_row(row: &Value) -> Result<XaiListedModel, XaiApiModelError> {
    let object = row
        .as_object()
        .ok_or_else(|| list_error("has a row that is not an object"))?;
    if let Some(lifecycle) = object.get("lifecycle").and_then(Value::as_str) {
        if lifecycle != "active" {
            return Err(list_error("has an unknown lifecycle"));
        }
    } else if object.contains_key("lifecycle") {
        return Err(list_error("has an unknown lifecycle"));
    }
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| list_error("has a row without an id"))?;
    let created = object
        .get("created")
        .and_then(Value::as_u64)
        .ok_or_else(|| list_error("has an unparseable timestamp"))?;
    let context_length = optional_u64(object.get("context_length"))?;
    let long_context_threshold = optional_u64(object.get("long_context_threshold"))?;
    let efforts = match object.get("capabilities") {
        None => Vec::new(),
        Some(capabilities) => {
            let capabilities = capabilities
                .as_object()
                .ok_or_else(|| list_error("has unparseable capabilities"))?;
            match capabilities.get("reasoning_effort") {
                None => Vec::new(),
                Some(Value::Array(names)) => names
                    .iter()
                    .map(|name| {
                        name.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| list_error("has an unparseable effort"))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => return Err(list_error("has an unparseable effort")),
            }
        }
    };
    Ok(XaiListedModel {
        id: id.to_owned(),
        created,
        context_length,
        long_context_threshold,
        efforts,
    })
}

fn optional_u64(value: Option<&Value>) -> Result<Option<u64>, XaiApiModelError> {
    match value {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| list_error("has an unparseable limit")),
    }
}

/// Lists models with one bounded authenticated `GET /v1/models` on the exact
/// Platform origin. The dial origin is not a parameter.
///
/// # Errors
/// Returns the credential, transport, status or parse failure.
pub async fn list_xai_api_models(
    source: &dyn XaiPlatformApiKeySource,
    observer: Option<std::sync::Arc<dyn plakous_http::ExchangeObserver>>,
) -> Result<Vec<XaiListedModel>, XaiApiModelError> {
    let origin =
        Origin::parse(XAI_PLATFORM_API_KEY_ORIGIN).map_err(|_| list_error("origin is invalid"))?;
    let client = Client::new_http1(plakous_http::Limits::default())
        .map_err(|_| transport_error())?
        .with_observer(observer);
    let key = platform_key(source)?;
    let body = get_models(&client, origin, &key, deadline_instant()?).await?;
    parse_xai_models_page(&body)
}

/// The real listing over a test transport (loopback dial and trust root).
#[cfg(test)]
async fn list_via(
    source: &dyn XaiPlatformApiKeySource,
    client: &Client,
    dial_origin: Origin,
) -> Result<Vec<XaiListedModel>, XaiApiModelError> {
    let key = platform_key(source)?;
    let body = get_models(client, dial_origin, &key, deadline_instant()?).await?;
    parse_xai_models_page(&body)
}

/// Non-secret 16-hex-digit fingerprint of the key the route would send.
///
/// # Errors
/// Returns an error when the credential cannot be resolved.
pub fn xai_api_key_fingerprint(
    source: &dyn XaiPlatformApiKeySource,
) -> Result<String, XaiApiModelError> {
    let grant = source
        .grant()
        .map_err(|_| list_error("credential is unavailable"))?;
    Ok(fingerprint_hex(grant.fingerprint.as_bytes()))
}

fn platform_key(source: &dyn XaiPlatformApiKeySource) -> Result<String, XaiApiModelError> {
    let grant = source
        .grant()
        .map_err(|_| list_error("credential is unavailable"))?;
    let key = source
        .api_key(&grant)
        .map_err(|_| list_error("credential is unavailable"))?;
    let header = key
        .bearer()
        .map_err(|_| list_error("credential is not a header value"))?;
    let value = header
        .to_str()
        .map_err(|_| list_error("credential is not a header value"))?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| list_error("credential is not a header value"))?;
    Ok(token.to_owned())
}

pub(crate) async fn get_models(
    client: &Client,
    origin: Origin,
    key: &str,
    deadline: Instant,
) -> Result<Vec<u8>, XaiApiModelError> {
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
        XAI_MODELS_PATH,
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

fn deadline_instant() -> Result<Instant, XaiApiModelError> {
    Instant::now()
        .checked_add(LIST_TIMEOUT)
        .ok_or_else(|| list_error("deadline is invalid"))
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

fn classify(
    listed: &XaiListedModel,
    reviewed: Option<&BundledRow>,
) -> Result<XaiApiModel, XaiApiModelError> {
    let id = listed.id.as_str();
    if not_coding(id) {
        return Ok(row_from(
            listed,
            XaiApiModelStatus::NotCodingModel("image, video, speech or embedding model".to_owned()),
            reviewed,
        ));
    }
    let live_raises = reviewed.is_some_and(|row| live_exceeds(listed, row));
    let status = if live_raises {
        XaiApiModelStatus::NeedsReview(
            "the live list publishes a higher window, threshold or effort than the reviewed table"
                .to_owned(),
        )
    } else if not_single_agent(id) {
        XaiApiModelStatus::NeedsReview(
            "a non-reasoning or multi-agent variant is not the platform profile".to_owned(),
        )
    } else if id.starts_with("grok-build") {
        XaiApiModelStatus::NeedsReview("grok-build publishes no reasoning effort".to_owned())
    } else if USABLE_PROFILES.contains(&id) {
        match reviewed {
            None => XaiApiModelStatus::NeedsReview(
                "not in this build's reviewed xAI model table".to_owned(),
            ),
            Some(row) if narrowed_efforts(listed, row).is_empty() => {
                XaiApiModelStatus::NeedsReview("publishes no reasoning effort".to_owned())
            }
            Some(_) => XaiApiModelStatus::Usable,
        }
    } else if reviewed.is_none() {
        XaiApiModelStatus::NeedsReview("not in this build's reviewed xAI model table".to_owned())
    } else {
        XaiApiModelStatus::NeedsReview(
            "this Responses model has no reviewed xAI platform profile".to_owned(),
        )
    };
    Ok(row_from(listed, status, reviewed))
}

fn live_exceeds(listed: &XaiListedModel, reviewed: &BundledRow) -> bool {
    if listed
        .context_length
        .is_some_and(|live| live > reviewed.context_length)
    {
        return true;
    }
    if listed.long_context_threshold.is_some_and(|live| {
        reviewed
            .long_context_threshold
            .is_none_or(|reviewed| live > reviewed)
    }) {
        return true;
    }
    let reviewed_efforts = efforts_from(&reviewed.efforts);
    listed
        .efforts
        .iter()
        .any(|name| effort_from(name).is_none_or(|effort| !reviewed_efforts.contains(&effort)))
}

fn not_coding(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    NOT_CODING_MARKERS.iter().any(|marker| id.contains(marker))
}

fn not_single_agent(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.contains("non-reasoning") || id.contains("multi-agent")
}

/// The facts one listed id runs under: the reviewed row narrowed by the live
/// row (the smaller context and threshold, the live efforts that are also
/// reviewed). Only a usable id carries efforts.
fn row_from(
    listed: &XaiListedModel,
    status: XaiApiModelStatus,
    reviewed: Option<&BundledRow>,
) -> XaiApiModel {
    let context_length = reviewed.map_or(0, |row| {
        listed
            .context_length
            .map_or(row.context_length, |live| live.min(row.context_length))
    });
    let price_threshold_input_tokens =
        reviewed
            .and_then(|row| row.long_context_threshold)
            .map(|threshold| {
                listed
                    .long_context_threshold
                    .map_or(threshold, |live| live.min(threshold))
            });
    let efforts = match (&status, reviewed) {
        (XaiApiModelStatus::Usable, Some(row)) => narrowed_efforts(listed, row),
        _ => Vec::new(),
    };
    XaiApiModel {
        id: listed.id.clone(),
        status,
        efforts,
        context_length,
        price_threshold_input_tokens,
        max_tokens: output_reserve(context_length),
    }
}

/// The reviewed ladder, cut to the live efforts when the live row publishes any.
fn narrowed_efforts(listed: &XaiListedModel, reviewed: &BundledRow) -> Vec<ReasoningEffort> {
    let reviewed = efforts_from(&reviewed.efforts);
    if listed.efforts.is_empty() {
        return reviewed;
    }
    reviewed
        .into_iter()
        .filter(|effort| {
            listed
                .efforts
                .iter()
                .any(|name| name == effort_name(*effort))
        })
        .collect()
}

fn efforts_from(names: &[String]) -> Vec<ReasoningEffort> {
    EFFORT_LADDER
        .iter()
        .filter(|(name, _)| names.iter().any(|listed| listed == name))
        .map(|(_, effort)| *effort)
        .collect()
}

fn effort_from(name: &str) -> Option<ReasoningEffort> {
    EFFORT_LADDER
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, effort)| *effort)
}

fn effort_name(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "none",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
        _ => "other",
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b':'))
}

#[derive(Debug, Deserialize)]
struct BundledTable {
    #[allow(dead_code, reason = "review metadata the census script rewrites")]
    revision: String,
    models: Vec<BundledRow>,
}

#[derive(Debug, Deserialize)]
struct BundledRow {
    id: String,
    context_length: u64,
    long_context_threshold: Option<u64>,
    efforts: Vec<String>,
}

fn bundled_table() -> Result<&'static BundledTable, XaiApiModelError> {
    static TABLE: std::sync::OnceLock<Result<BundledTable, XaiApiModelError>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| parse_table(BUNDLED_TABLE))
        .as_ref()
        .map_err(Clone::clone)
}

/// Parses a reviewed table. An effort name outside the ladder is a table
/// error, never a silently dropped level.
fn parse_table(text: &str) -> Result<BundledTable, XaiApiModelError> {
    let table: BundledTable = serde_json::from_str(text).map_err(|_| table_error())?;
    let unknown_effort = table
        .models
        .iter()
        .flat_map(|row| &row.efforts)
        .any(|name| effort_from(name).is_none());
    if unknown_effort {
        return Err(table_error());
    }
    Ok(table)
}

fn table_error() -> XaiApiModelError {
    list_error("reviewed table is malformed")
}

fn list_error(reason: &str) -> XaiApiModelError {
    XaiApiModelError(format!("xAI model list {reason}"))
}

fn transport_error() -> XaiApiModelError {
    list_error("request failed")
}

/// Listing or table failure. The message names the reason and no credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XaiApiModelError(String);

impl std::fmt::Display for XaiApiModelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for XaiApiModelError {}

impl From<XaiPlatformBuildError> for XaiApiModelError {
    fn from(_: XaiPlatformBuildError) -> Self {
        list_error("credential is unavailable")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
#[path = "api_models_tests.rs"]
mod tests;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "loopback listing tests assert the transport"
)]
#[path = "api_models_live_tests.rs"]
mod live_tests;
