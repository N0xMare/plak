//! `web_fetch`: the hosted page fetch wrapper.

use serde::Deserialize;
use serde_json::{Value, json};

use plakous_tools::{CodingPresentation, RetrievalRequest, ToolRuntimeError, object_schema};

use crate::text::PLAKOUS_WEB_CLAUDE_FETCH_DESCRIPTION;
use crate::{MAX_PROMPT_BYTES, MAX_URL_BYTES, Shape, invalid, require_text};

/// Only the Claude vocabulary presents a hosted fetch.
pub(crate) fn shape(presentation: CodingPresentation) -> Option<Shape> {
    match presentation {
        CodingPresentation::Claude => Some(Shape {
            name: "WebFetch",
            description: PLAKOUS_WEB_CLAUDE_FETCH_DESCRIPTION,
            schema: object_schema(
                json!({
                    "url": {"type": "string", "minLength": 8, "maxLength": MAX_URL_BYTES},
                    "prompt": {"type": "string", "minLength": 1, "maxLength": MAX_PROMPT_BYTES}
                }),
                &["url", "prompt"],
            ),
        }),
        CodingPresentation::Codex | CodingPresentation::Grok | CodingPresentation::Pi => None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    url: String,
    prompt: Option<String>,
}

pub(crate) fn request(arguments: Value) -> Result<RetrievalRequest, ToolRuntimeError> {
    let arguments: Arguments = serde_json::from_value(arguments)
        .map_err(|_| invalid("web fetch arguments are invalid"))?;
    let url = require_text(&arguments.url, MAX_URL_BYTES, "url")?;
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| invalid("url must be an absolute http or https URL"))?;
    if rest.is_empty() || rest.starts_with('/') || url.chars().any(char::is_whitespace) {
        return Err(invalid("url must be an absolute http or https URL"));
    }
    let prompt = arguments
        .prompt
        .map(|prompt| require_text(&prompt, MAX_PROMPT_BYTES, "prompt"))
        .transpose()?;
    Ok(RetrievalRequest::PageFetch { url, prompt })
}

/// The URL's host, for the presentation subject.
pub(crate) fn host(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    authority.rsplit('@').next().unwrap_or(authority)
}
