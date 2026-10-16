//! `x_search`: the hosted X post search wrapper.

use serde::Deserialize;
use serde_json::{Value, json};

use plakous_tools::{CodingPresentation, RetrievalRequest, ToolRuntimeError, object_schema};

use crate::text::PLAKOUS_WEB_X_SEARCH_DESCRIPTION;
use crate::{MAX_DOMAINS, MAX_QUERY_BYTES, Shape, invalid, require_text};

/// Only the Grok vocabulary presents X search.
pub(crate) fn shape(presentation: CodingPresentation) -> Option<Shape> {
    match presentation {
        CodingPresentation::Grok => Some(Shape {
            name: "x_search",
            description: PLAKOUS_WEB_X_SEARCH_DESCRIPTION,
            schema: object_schema(
                json!({
                    "query": {"type": "string", "minLength": 2, "maxLength": MAX_QUERY_BYTES},
                    "handles": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1, "maxLength": 15},
                        "maxItems": MAX_DOMAINS
                    },
                    "from_date": {"type": "string", "minLength": 10, "maxLength": 10},
                    "to_date": {"type": "string", "minLength": 10, "maxLength": 10}
                }),
                &["query"],
            ),
        }),
        CodingPresentation::Codex | CodingPresentation::Claude | CodingPresentation::Pi => None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    query: String,
    #[serde(default)]
    handles: Vec<String>,
    from_date: Option<String>,
    to_date: Option<String>,
}

pub(crate) fn request(arguments: Value) -> Result<RetrievalRequest, ToolRuntimeError> {
    let arguments: Arguments =
        serde_json::from_value(arguments).map_err(|_| invalid("X search arguments are invalid"))?;
    let query = require_text(&arguments.query, MAX_QUERY_BYTES, "query")?;
    if arguments.handles.len() > MAX_DOMAINS {
        return Err(invalid("too many handles"));
    }
    let handles = arguments
        .handles
        .into_iter()
        .map(|handle| {
            let handle = handle.trim().trim_start_matches('@').to_owned();
            let valid = (1..=15).contains(&handle.len())
                && handle
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            if valid {
                Ok(handle)
            } else {
                Err(invalid("a handle is not an X handle"))
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(RetrievalRequest::PostSearch {
        query,
        handles,
        from_date: arguments.from_date.map(date).transpose()?,
        to_date: arguments.to_date.map(date).transpose()?,
    })
}

fn date(value: String) -> Result<String, ToolRuntimeError> {
    let bytes = value.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit());
    if shaped {
        Ok(value)
    } else {
        Err(invalid("dates are YYYY-MM-DD"))
    }
}
