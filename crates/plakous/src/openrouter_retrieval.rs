//! The neutral retrieval port over the OpenRouter side-request executor.

use std::sync::Arc;

use plakous_openrouter::{
    ChatRetrievalError, ChatRetrievalErrorKind, ChatRetrievalOutcome, OpenRouterRetrieval,
};
use plakous_tools::{
    ExecutionOperationControl, RetrievalError, RetrievalErrorKind, RetrievalFuture, RetrievalKind,
    RetrievalOutcome, RetrievalPort, RetrievalRequest, RetrievalSource, RetrievalUsage,
};

const KINDS: [RetrievalKind; 1] = [RetrievalKind::WebSearch];

/// One session's OpenRouter hosted web search as a [`RetrievalPort`].
pub struct OpenRouterRetrievalPort(Arc<OpenRouterRetrieval>);

impl OpenRouterRetrievalPort {
    /// Wraps the session's side-request executor.
    #[must_use]
    pub fn new(retrieval: OpenRouterRetrieval) -> Self {
        Self(Arc::new(retrieval))
    }
}

impl RetrievalPort for OpenRouterRetrievalPort {
    fn kinds(&self) -> &[RetrievalKind] {
        &KINDS
    }

    /// One exchange within the turn retry budget: see the constant.
    fn max_requests(&self, kind: RetrievalKind) -> Option<u32> {
        KINDS
            .contains(&kind)
            .then_some(plakous_openrouter::MAX_CHAT_RETRIEVAL_PHYSICAL_REQUESTS)
    }

    fn retrieve(
        &self,
        request: RetrievalRequest,
        control: ExecutionOperationControl,
    ) -> RetrievalFuture {
        let retrieval = Arc::clone(&self.0);
        Box::pin(async move {
            let RetrievalRequest::WebSearch {
                query,
                allowed_domains,
                blocked_domains,
                ..
            } = request
            else {
                return Err(RetrievalError::new(
                    RetrievalErrorKind::Unsupported,
                    "this route only searches the web",
                ));
            };
            let request = retrieval.request(&query, allowed_domains, blocked_domains);
            retrieval
                .retrieve(&request, control.cancellation())
                .await
                .map(outcome)
                .map_err(|error| error_of(&error))
        })
    }
}

fn outcome(outcome: ChatRetrievalOutcome) -> RetrievalOutcome {
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
        physical_requests: Some(outcome.physical_requests),
    }
}

fn error_of(error: &ChatRetrievalError) -> RetrievalError {
    let kind = match error.kind() {
        ChatRetrievalErrorKind::InvalidRequest => RetrievalErrorKind::InvalidRequest,
        ChatRetrievalErrorKind::Unauthorized => RetrievalErrorKind::Unauthorized,
        ChatRetrievalErrorKind::Rejected => RetrievalErrorKind::Rejected,
        ChatRetrievalErrorKind::Cancelled => RetrievalErrorKind::Cancelled,
        ChatRetrievalErrorKind::InvalidResponse => RetrievalErrorKind::InvalidResponse,
        _ => RetrievalErrorKind::Unavailable,
    };
    RetrievalError::new(kind, error.message())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_and_errors_map_onto_the_neutral_port() {
        let mapped = outcome(ChatRetrievalOutcome {
            text: "answer".to_owned(),
            sources: vec![plakous_openrouter::ChatRetrievalSource {
                url: "https://example.com/".to_owned(),
                title: None,
            }],
            usage: plakous_openrouter::ChatRetrievalUsage {
                input_tokens: 3,
                output_tokens: 2,
                hosted_requests: 1,
            },
            physical_requests: 2,
        });
        assert_eq!(mapped.physical_requests, Some(2));
        assert_eq!(mapped.text, "answer");
        assert_eq!(mapped.sources[0].url, "https://example.com/");
        assert_eq!(mapped.usage.hosted_requests, 1);
        let refused = error_of(&ChatRetrievalError::new(
            ChatRetrievalErrorKind::Rejected,
            "the provider refused the side request (HTTP 400)",
        ));
        assert_eq!(refused.kind(), RetrievalErrorKind::Rejected);
        assert_eq!(
            refused.message(),
            "the provider refused the side request (HTTP 400)"
        );
    }
}
