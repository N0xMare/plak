//! The neutral retrieval port over the Anthropic API and Claude OAuth
//! side-request executor.

use std::sync::Arc;

use plakous_anthropic::{
    AnthropicRetrieval, MessagesRetrievalError, MessagesRetrievalErrorKind,
    MessagesRetrievalOutcome, MessagesRetrievalQuery,
};
use plakous_tools::{
    ExecutionOperationControl, RetrievalError, RetrievalErrorKind, RetrievalFuture, RetrievalKind,
    RetrievalOutcome, RetrievalPort, RetrievalRequest, RetrievalSource, RetrievalUsage,
};

const KINDS: [RetrievalKind; 2] = [RetrievalKind::WebSearch, RetrievalKind::PageFetch];

/// One session's Anthropic or Claude hosted web search and page fetch as a
/// [`RetrievalPort`].
pub struct AnthropicRetrievalPort(Arc<AnthropicRetrieval>);

impl AnthropicRetrievalPort {
    /// Wraps the session's side-request executor.
    #[must_use]
    pub fn new(retrieval: AnthropicRetrieval) -> Self {
        Self(Arc::new(retrieval))
    }
}

impl RetrievalPort for AnthropicRetrievalPort {
    fn kinds(&self) -> &[RetrievalKind] {
        &KINDS
    }

    fn retrieve(
        &self,
        request: RetrievalRequest,
        control: ExecutionOperationControl,
    ) -> RetrievalFuture {
        let retrieval = Arc::clone(&self.0);
        Box::pin(async move {
            let query = query(request)?;
            retrieval
                .retrieve(query, control.cancellation())
                .await
                .map(outcome)
                .map_err(|error| error_of(&error))
        })
    }
}

fn query(request: RetrievalRequest) -> Result<MessagesRetrievalQuery, RetrievalError> {
    match request {
        RetrievalRequest::WebSearch {
            query,
            allowed_domains,
            blocked_domains,
        } => Ok(MessagesRetrievalQuery::WebSearch {
            query,
            allowed_domains,
            blocked_domains,
        }),
        RetrievalRequest::PageFetch { url, prompt } => {
            Ok(MessagesRetrievalQuery::PageFetch { url, prompt })
        }
        RetrievalRequest::PostSearch { .. } => Err(RetrievalError::new(
            RetrievalErrorKind::Unsupported,
            "this route does not search posts",
        )),
    }
}

fn outcome(outcome: MessagesRetrievalOutcome) -> RetrievalOutcome {
    RetrievalOutcome {
        text: outcome.text,
        sources: outcome
            .sources
            .into_iter()
            .map(|source| RetrievalSource {
                url: source.url,
                title: source.title,
            })
            .collect(),
        usage: RetrievalUsage {
            input_tokens: outcome.usage.input_tokens,
            output_tokens: outcome.usage.output_tokens,
            hosted_requests: outcome.usage.hosted_requests,
        },
    }
}

fn error_of(error: &MessagesRetrievalError) -> RetrievalError {
    let kind = match error.kind() {
        MessagesRetrievalErrorKind::InvalidRequest => RetrievalErrorKind::InvalidRequest,
        MessagesRetrievalErrorKind::Unauthorized => RetrievalErrorKind::Unauthorized,
        MessagesRetrievalErrorKind::Rejected => RetrievalErrorKind::Rejected,
        MessagesRetrievalErrorKind::Cancelled => RetrievalErrorKind::Cancelled,
        MessagesRetrievalErrorKind::InvalidResponse => RetrievalErrorKind::InvalidResponse,
        _ => RetrievalErrorKind::Unavailable,
    };
    RetrievalError::new(kind, error.message())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_errors_map_onto_the_side_request() {
        assert_eq!(
            query(RetrievalRequest::PageFetch {
                url: "https://example.com/".to_owned(),
                prompt: None,
            }),
            Ok(MessagesRetrievalQuery::PageFetch {
                url: "https://example.com/".to_owned(),
                prompt: None,
            })
        );
        let refused = query(RetrievalRequest::PostSearch {
            query: "q".to_owned(),
            handles: Vec::new(),
            from_date: None,
            to_date: None,
        });
        assert_eq!(
            refused.map_err(|error| error.kind()),
            Err(RetrievalErrorKind::Unsupported)
        );
        let mapped = error_of(&MessagesRetrievalError::new(
            MessagesRetrievalErrorKind::Rejected,
            "the provider refused the side request (HTTP 400)",
        ));
        assert_eq!(mapped.kind(), RetrievalErrorKind::Rejected);
        assert_eq!(
            mapped.message(),
            "the provider refused the side request (HTTP 400)"
        );
    }
}
