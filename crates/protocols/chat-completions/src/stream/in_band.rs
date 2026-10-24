//! In-band error statuses and redacted summaries carried in stream chunks.

/// Reads the HTTP-shaped status an in-band provider error named, if any.
///
/// A gateway puts the upstream status in `error.code`, as a number or as its
/// decimal string, and that is where this looks first.
///
/// It also reads one prose spelling, and only one: an edge that could not reach
/// its upstream relays that upstream's own body, and the body says
/// `error code: 520` and nothing else machine-readable. The live shape this
/// exists for is exactly that — the diagnostic recorded in the field was
/// "Chat provider returned an in-band error: error code: 520" — so the status
/// was there, in the one place nothing could read it. Recognising that anchored
/// spelling is narrow on purpose: it is not general prose parsing, and anything
/// that does not match leaves the status unknown and the conservative
/// classification in place.
pub(super) fn in_band_error_status(error: &serde_json::Value) -> Option<u16> {
    if let Some(code) = error.get("code") {
        // Every arm falls through rather than propagating: a `code` that is a
        // float, a word, or out of range is not an answer, and the message may
        // still hold one.
        let status = match code {
            serde_json::Value::Number(number) => {
                number.as_u64().and_then(|value| u16::try_from(value).ok())
            }
            serde_json::Value::String(text) => text.trim().parse::<u16>().ok(),
            _ => None,
        };
        if let Some(status) = status.filter(|status| (100..=599).contains(status)) {
            return Some(status);
        }
    }
    let message = match error {
        serde_json::Value::String(text) => text,
        serde_json::Value::Object(object) => object.get("message")?.as_str()?,
        _ => return None,
    };
    edge_error_code_status(message)
}

/// Reads `error code: NNN` from an edge body, anchored and nothing else.
fn edge_error_code_status(message: &str) -> Option<u16> {
    const ANCHOR: &str = "error code:";
    let lowered = message.to_ascii_lowercase();
    let start = lowered.find(ANCHOR)? + ANCHOR.len();
    let digits: String = message
        .get(start..)?
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let status = digits.parse::<u16>().ok()?;
    (100..=599).contains(&status).then_some(status)
}

const IN_BAND_ERROR_DETAIL_CHARS: usize = 240;
const IN_BAND_UPSTREAM_CHARS: usize = 64;

/// Composes what a well-formed in-band error object said.
///
/// A named status leads, as `HTTP 504 (timeout)`, with the class that status
/// is typed as before acceptance, so both read alike; exhausted quota names no
/// refusal class. The upstream the gateway named follows, then the provider's
/// message unless it only restates the status (`error code: 504`). Without a
/// status the provider's message is the whole account.
pub(super) fn in_band_error_summary(
    error: &serde_json::Value,
    status: Option<u16>,
    usage_limit: bool,
    chunk_provider: Option<&serde_json::Value>,
) -> String {
    let upstream = in_band_upstream(error, chunk_provider);
    let Some(status) = status else {
        let detail = in_band_error_detail(error);
        return match upstream {
            Some(upstream) => format!("{detail} (from {upstream})"),
            None => detail,
        };
    };
    let mut summary = format!("HTTP {status}");
    if let Some(refusal) = plakous_attempt::ProviderRefusal::from_http_status(status)
        && !usage_limit
    {
        summary = format!("{summary} ({})", refusal.label());
    }
    if let Some(upstream) = upstream {
        summary = format!("{summary} from {upstream}");
    }
    match in_band_status_detail(error, status) {
        Some(detail) => format!("{summary}: {detail}"),
        None => summary,
    }
}

/// What an in-band error with a known status adds to it: its message, or
/// without one its `type` label. `None` when it only restates the status.
fn in_band_status_detail(error: &serde_json::Value, status: u16) -> Option<String> {
    let raw = match error {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(object) => {
            let field = |key| {
                object
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
            };
            match (field("message"), field("type")) {
                (Some(message), _) => message.to_owned(),
                (None, Some(label)) => format!("type={label}"),
                (None, None) => return None,
            }
        }
        _ => return None,
    };
    let detail = bounded_in_band_text(&raw, IN_BAND_ERROR_DETAIL_CHARS);
    let restated =
        detail.is_empty() || detail.eq_ignore_ascii_case(&format!("error code: {status}"));
    (!restated).then_some(detail)
}

/// The upstream a gateway named for its in-band error: its error metadata
/// first, then the chunk's own `provider`. Bounded; never the raw upstream body.
fn in_band_upstream(
    error: &serde_json::Value,
    chunk_provider: Option<&serde_json::Value>,
) -> Option<String> {
    let name = error
        .pointer("/metadata/provider_name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| chunk_provider.and_then(serde_json::Value::as_str))?;
    let name = bounded_in_band_text(name, IN_BAND_UPSTREAM_CHARS);
    (!name.is_empty()).then_some(name)
}

/// Provider text without control or format characters (bidirectional
/// overrides, zero-width marks), bounded and trimmed, so it cannot reorder or
/// hide the operator text around it.
fn bounded_in_band_text(raw: &str, limit: usize) -> String {
    let sanitized: String = raw
        .chars()
        .filter(|ch| {
            *ch == ' '
                || !(ch.is_control()
                    || unicode_general_category::get_general_category(*ch)
                        == unicode_general_category::GeneralCategory::Format)
        })
        .take(limit)
        .collect();
    sanitized.trim().to_owned()
}

fn in_band_error_detail(error: &serde_json::Value) -> String {
    let raw = match error {
        serde_json::Value::Object(object) => in_band_error_object_detail(object),
        serde_json::Value::String(text) => text.trim().to_owned(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null | serde_json::Value::Array(_) => String::new(),
    };
    let detail = bounded_in_band_text(&raw, IN_BAND_ERROR_DETAIL_CHARS);
    if detail.is_empty() {
        "unclassified".to_owned()
    } else {
        detail
    }
}

fn in_band_error_object_detail(object: &serde_json::Map<String, serde_json::Value>) -> String {
    if let Some(message) = object
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return message.to_owned();
    }
    let mut parts = Vec::new();
    for key in ["code", "type"] {
        match object.get(key) {
            Some(serde_json::Value::String(text)) => {
                let text = text.trim();
                if !text.is_empty() {
                    parts.push(format!("{key}={text}"));
                }
            }
            Some(serde_json::Value::Number(number)) => {
                parts.push(format!("{key}={number}"));
            }
            Some(serde_json::Value::Object(nested)) => {
                if let Some(message) = nested
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                {
                    parts.push(format!("{key}={message}"));
                }
            }
            _ => {}
        }
    }
    parts.join("; ")
}
