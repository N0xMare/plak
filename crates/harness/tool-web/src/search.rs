//! `web_search`: the hosted web search wrapper.

use serde::Deserialize;
use serde_json::{Value, json};

use plakous_tools::{CodingPresentation, RetrievalRequest, ToolRuntimeError, object_schema};

use crate::text::{PLAKOUS_WEB_CLAUDE_SEARCH_DESCRIPTION, PLAKOUS_WEB_SEARCH_DESCRIPTION};
use crate::{MAX_DOMAINS, MAX_QUERY_BYTES, Shape, invalid, require_text};

pub(crate) fn shape(presentation: CodingPresentation) -> Shape {
    match presentation {
        CodingPresentation::Claude => Shape {
            name: "WebSearch",
            description: PLAKOUS_WEB_CLAUDE_SEARCH_DESCRIPTION,
            schema: object_schema(
                json!({
                    "query": {"type": "string", "minLength": 2, "maxLength": MAX_QUERY_BYTES},
                    "allowed_domains": domains_schema(),
                    "blocked_domains": domains_schema()
                }),
                &["query"],
            ),
        },
        CodingPresentation::Codex | CodingPresentation::Grok | CodingPresentation::Pi => Shape {
            name: "web_search",
            description: PLAKOUS_WEB_SEARCH_DESCRIPTION,
            schema: object_schema(
                json!({
                    "query": {"type": "string", "minLength": 2, "maxLength": MAX_QUERY_BYTES}
                }),
                &["query"],
            ),
        },
    }
}

fn domains_schema() -> Value {
    json!({
        "type": "array",
        "items": {"type": "string", "minLength": 1, "maxLength": 253},
        "maxItems": MAX_DOMAINS
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    query: String,
    #[serde(default)]
    allowed_domains: Vec<String>,
    #[serde(default)]
    blocked_domains: Vec<String>,
}

pub(crate) fn request(arguments: Value) -> Result<RetrievalRequest, ToolRuntimeError> {
    let arguments: Arguments = serde_json::from_value(arguments)
        .map_err(|_| invalid("web search arguments are invalid"))?;
    let query = require_text(&arguments.query, MAX_QUERY_BYTES, "query")?;
    if !arguments.allowed_domains.is_empty() && !arguments.blocked_domains.is_empty() {
        return Err(invalid("set allowed_domains or blocked_domains, not both"));
    }
    Ok(RetrievalRequest::WebSearch {
        query,
        allowed_domains: domains(arguments.allowed_domains)?,
        blocked_domains: domains(arguments.blocked_domains)?,
    })
}

fn domains(values: Vec<String>) -> Result<Vec<String>, ToolRuntimeError> {
    if values.len() > MAX_DOMAINS {
        return Err(invalid("too many domains"));
    }
    values
        .into_iter()
        .map(|domain| {
            let domain = domain.trim().to_ascii_lowercase();
            let valid = !domain.is_empty()
                && domain.len() <= 253
                && domain
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
            if valid {
                Ok(domain)
            } else {
                Err(invalid("a domain is not a host name"))
            }
        })
        .collect()
}
