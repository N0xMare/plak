//! The Codex `alpha/search` side request: body, response mapping and, with
//! native auth, the executor that sends it.
//!
//! Codex Responses Lite rows carry no hosted tools, so the reference client
//! searches through a separate `<base>/alpha/search` request. This module
//! owns that request's exact body, its cost-only allowlist, the mapping of
//! its `results[]` onto sources, and [`CodexRetrieval`], which sends it under
//! the session's own subscription login with a model round's admission
//! revalidation, credential preflight and inference permit per physical
//! request.

use std::collections::BTreeSet;
#[cfg(feature = "native-auth")]
use std::fmt;
#[cfg(feature = "native-auth")]
use std::sync::Arc;

use plakous_attempt::ReplaySafety;
#[cfg(feature = "native-auth")]
use plakous_model::CancellationToken;
use plakous_responses_dialect::{
    RESPONSES_RETRIEVAL_MAX_OUTPUT_TOKENS, ResponsesRetrievalError, ResponsesRetrievalErrorKind,
    ResponsesRetrievalOutcome, ResponsesRetrievalSource, ResponsesRetrievalUsage,
};
#[cfg(feature = "native-auth")]
use plakous_responses_dialect::{
    ResponsesRetrievalAuthorization, ResponsesRetrievalDispatch, ResponsesRetrievalLimits,
    ResponsesRetrievalQuery, ResponsesRetrievalTransport, run_responses_json_side_request,
};
use serde_json::{Value, json};

#[cfg(feature = "native-auth")]
use crate::OpenAiLanguageModel;

/// The search endpoint path under the Codex backend base.
pub const CODEX_SEARCH_PATH: &str = "/backend-api/codex/alpha/search";

const MAX_RESULTS: usize = 64;

/// One Codex web search side request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexSearchRequest {
    id: String,
    model: String,
    query: String,
}

impl CodexSearchRequest {
    /// A search for `query` on the session's `model`; `id` is the fresh
    /// request id.
    #[must_use]
    pub fn new(id: impl Into<String>, model: impl Into<String>, query: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            query: query.into(),
        }
    }

    /// The exact body: the query as the one user message and as the one
    /// `search_query` command.
    ///
    /// # Errors
    ///
    /// Fails only when serialization fails.
    pub fn body(&self) -> Result<Vec<u8>, ResponsesRetrievalError> {
        serde_json::to_vec(&json!({
            "id": self.id,
            "model": self.model,
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": self.query}],
            }],
            "commands": {"search_query": [{"q": self.query}]},
            "max_output_tokens": RESPONSES_RETRIEVAL_MAX_OUTPUT_TOKENS,
        }))
        .map_err(|_| {
            ResponsesRetrievalError::new(
                ResponsesRetrievalErrorKind::InvalidRequest,
                "side request could not be encoded",
            )
        })
    }
}

/// Classifies a Codex search body: only the known fields, and commands that
/// are exactly one or more `search_query` entries carrying only `q`, are
/// [`ReplaySafety::CostOnly`].
#[must_use]
pub fn classify_codex_search_body(body: &[u8]) -> ReplaySafety {
    const FIELDS: [&str; 6] = [
        "id",
        "model",
        "input",
        "commands",
        "settings",
        "max_output_tokens",
    ];
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(body) else {
        return ReplaySafety::ServerEffect;
    };
    if body.keys().any(|key| !FIELDS.contains(&key.as_str())) {
        return ReplaySafety::ServerEffect;
    }
    let Some(Value::Object(commands)) = body.get("commands") else {
        return ReplaySafety::ServerEffect;
    };
    let only_queries = commands.len() == 1
        && commands
            .get("search_query")
            .and_then(Value::as_array)
            .is_some_and(|queries| {
                !queries.is_empty()
                    && queries.iter().all(|query| {
                        query.as_object().is_some_and(|query| {
                            query.len() == 1 && query.get("q").is_some_and(Value::is_string)
                        })
                    })
            });
    if only_queries {
        ReplaySafety::CostOnly
    } else {
        ReplaySafety::ServerEffect
    }
}

/// Maps a Codex search response: `output` is the answer, `results[]` the
/// sources; `encrypted_output` is dropped and never retained.
///
/// # Errors
///
/// Fails when the body is not the search response grammar.
pub fn decode_codex_search_response(
    body: &[u8],
) -> Result<ResponsesRetrievalOutcome, ResponsesRetrievalError> {
    let invalid = || {
        ResponsesRetrievalError::new(
            ResponsesRetrievalErrorKind::InvalidResponse,
            "the search response broke its grammar",
        )
    };
    let Ok(Value::Object(response)) = serde_json::from_slice::<Value>(body) else {
        return Err(invalid());
    };
    let text = match response.get("output") {
        Some(Value::String(text)) => text.clone(),
        None | Some(Value::Null) => String::new(),
        Some(_) => return Err(invalid()),
    };
    let results = match response.get("results") {
        Some(Value::Array(results)) if results.len() <= MAX_RESULTS => results.as_slice(),
        None | Some(Value::Null) => &[],
        Some(_) => return Err(invalid()),
    };
    let mut seen = BTreeSet::new();
    let mut sources = Vec::new();
    for result in results {
        let url = result
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .ok_or_else(invalid)?;
        if seen.insert(url.to_owned()) {
            sources.push(ResponsesRetrievalSource {
                url: url.to_owned(),
                title: result
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned),
            });
        }
    }
    if text.trim().is_empty() && sources.is_empty() {
        return Err(ResponsesRetrievalError::new(
            ResponsesRetrievalErrorKind::Rejected,
            "the provider's search returned nothing",
        ));
    }
    Ok(ResponsesRetrievalOutcome {
        text,
        sources,
        usage: ResponsesRetrievalUsage {
            hosted_requests: 1,
            ..ResponsesRetrievalUsage::default()
        },
    })
}

/// One session's hosted web search on the Codex subscription route.
#[cfg(feature = "native-auth")]
pub struct CodexRetrieval {
    model: Arc<OpenAiLanguageModel>,
    limits: ResponsesRetrievalLimits,
}

#[cfg(feature = "native-auth")]
impl CodexRetrieval {
    pub(crate) fn new(model: Arc<OpenAiLanguageModel>) -> Self {
        Self {
            model,
            limits: ResponsesRetrievalLimits::default(),
        }
    }

    /// Replaces the exchange bounds and backoff sample.
    #[must_use]
    pub const fn with_limits(mut self, limits: ResponsesRetrievalLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The exact side request for `query` under the fresh request `id`.
    ///
    /// # Errors
    ///
    /// Refuses a query the search command cannot express: post search and
    /// domain filters.
    pub fn request(
        &self,
        id: impl Into<String>,
        query: ResponsesRetrievalQuery,
    ) -> Result<CodexSearchRequest, ResponsesRetrievalError> {
        if let Some(refused) = refuse_unexpressible(&query) {
            return Err(refused);
        }
        let (ResponsesRetrievalQuery::WebSearch { query, .. }
        | ResponsesRetrievalQuery::PostSearch { query, .. }) = query;
        Ok(CodexSearchRequest::new(
            id,
            self.model.descriptor().requested_model().as_str(),
            query,
        ))
    }

    /// Runs one side request.
    ///
    /// # Errors
    ///
    /// Returns the side request's failure after its bounded retries.
    pub async fn retrieve(
        &self,
        query: ResponsesRetrievalQuery,
        cancellation: &CancellationToken,
    ) -> Result<ResponsesRetrievalOutcome, ResponsesRetrievalError> {
        let id = plakous_model::AttemptId::generate(
            &plakous_model::SystemClock,
            &plakous_model::SystemIdGenerator,
        )
        .map_err(|_| {
            ResponsesRetrievalError::new(
                ResponsesRetrievalErrorKind::InvalidRequest,
                "side request id could not be generated",
            )
        })?;
        let body = self.request(id.to_string(), query)?.body()?;
        let transport = CodexSideRequest {
            retrieval: self,
            cancellation: cancellation.clone(),
        };
        let document = run_responses_json_side_request(
            self.model.client(),
            &transport,
            &body,
            classify_codex_search_body,
            cancellation,
            self.limits,
        )
        .await?;
        decode_codex_search_response(&document)
    }
}

/// Refuses what the search command cannot express: post search and domain
/// filters.
#[cfg(feature = "native-auth")]
pub(crate) fn refuse_unexpressible(
    query: &ResponsesRetrievalQuery,
) -> Option<ResponsesRetrievalError> {
    let refused = match query {
        ResponsesRetrievalQuery::WebSearch {
            allowed_domains,
            blocked_domains,
            ..
        } if allowed_domains.is_empty() && blocked_domains.is_empty() => return None,
        ResponsesRetrievalQuery::WebSearch { .. } => {
            "this route's web search cannot filter domains"
        }
        ResponsesRetrievalQuery::PostSearch { .. } => "this route does not search posts",
    };
    Some(ResponsesRetrievalError::new(
        ResponsesRetrievalErrorKind::InvalidRequest,
        refused,
    ))
}

/// One logical side request's transport: the caller's cancellation also
/// bounds each physical request's credential preflight.
#[cfg(feature = "native-auth")]
struct CodexSideRequest<'a> {
    retrieval: &'a CodexRetrieval,
    cancellation: CancellationToken,
}

#[cfg(feature = "native-auth")]
impl ResponsesRetrievalTransport for CodexSideRequest<'_> {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a> {
        Box::pin(async move {
            let deadline = std::time::Instant::now() + self.retrieval.limits.exchange_timeout;
            let (request, guard) = self
                .retrieval
                .model
                .codex_side_request(body, deadline, &self.cancellation)
                .await
                .map_err(|message| {
                    ResponsesRetrievalError::new(
                        ResponsesRetrievalErrorKind::Unauthorized,
                        format!("side request authority is not current: {message}"),
                    )
                })?;
            Ok(ResponsesRetrievalDispatch::new(request).with_guard(guard))
        })
    }
}

#[cfg(feature = "native-auth")]
impl fmt::Debug for CodexRetrieval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexRetrieval")
            .field("route", &"codex")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn the_search_body_is_exact_and_cost_only() -> TestResult {
        let body = CodexSearchRequest::new("req-1", "gpt-5.5-codex", "rust 1.99").body()?;
        assert_eq!(
            String::from_utf8(body.clone())?,
            concat!(
                r#"{"commands":{"search_query":[{"q":"rust 1.99"}]},"id":"req-1","input":[{"content":"#,
                r#"[{"text":"rust 1.99","type":"input_text"}],"role":"user","type":"message"}],"#,
                r#""max_output_tokens":4096,"model":"gpt-5.5-codex"}"#,
            )
        );
        assert_eq!(classify_codex_search_body(&body), ReplaySafety::CostOnly);
        for changed in [
            r#"{"id":"r","model":"m","input":[],"commands":{"open":[{"ref_id":"x"}]}}"#,
            r#"{"id":"r","model":"m","input":[],"commands":{"search_query":[{"q":"x","recency":1}]}}"#,
            r#"{"id":"r","model":"m","input":[],"commands":{"search_query":[]}}"#,
            r#"{"id":"r","model":"m","input":[],"commands":{"search_query":[{"q":"x"}]},"tools":[]}"#,
        ] {
            assert_eq!(
                classify_codex_search_body(changed.as_bytes()),
                ReplaySafety::ServerEffect,
                "{changed}"
            );
        }
        Ok(())
    }

    #[test]
    fn results_map_to_unique_sources_and_encrypted_output_is_dropped() -> TestResult {
        let outcome = decode_codex_search_response(
            br#"{"output":"Rust 1.99 shipped.","encrypted_output":"gAAA","results":[
                {"url":"https://blog.rust-lang.org/a","title":"Rust 1.99","snippet":"s"},
                {"url":"https://blog.rust-lang.org/a","title":"dup"},
                {"url":"https://example.com/b","title":""}]}"#,
        )?;
        assert_eq!(outcome.text, "Rust 1.99 shipped.");
        assert_eq!(
            outcome.sources,
            [
                ResponsesRetrievalSource {
                    url: "https://blog.rust-lang.org/a".to_owned(),
                    title: Some("Rust 1.99".to_owned()),
                },
                ResponsesRetrievalSource {
                    url: "https://example.com/b".to_owned(),
                    title: None,
                },
            ]
        );
        assert_eq!(outcome.usage.hosted_requests, 1);
        Ok(())
    }

    #[test]
    fn an_empty_or_malformed_search_response_is_an_error() {
        assert_eq!(
            decode_codex_search_response(br#"{"output":"","results":[]}"#)
                .err()
                .map(|error| error.kind()),
            Some(ResponsesRetrievalErrorKind::Rejected)
        );
        for body in [
            &br#"{"output":1}"#[..],
            br#"{"output":"x","results":[{"title":"no url"}]}"#,
            b"not json",
        ] {
            assert_eq!(
                decode_codex_search_response(body)
                    .err()
                    .map(|error| error.kind()),
                Some(ResponsesRetrievalErrorKind::InvalidResponse)
            );
        }
    }
}
