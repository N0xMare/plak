//! The neutral retrieval port over the OpenRouter side-request executor.

use crate::retrieval_outcome::{error_of, outcome};
use std::sync::Arc;

use plakous_openrouter::OpenRouterRetrieval;
use plakous_tools::{
    ExecutionOperationControl, RetrievalError, RetrievalErrorKind, RetrievalFuture, RetrievalKind,
    RetrievalPort, RetrievalRequest,
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

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_openrouter::{ChatRetrievalError, ChatRetrievalErrorKind, ChatRetrievalOutcome};

    #[test]
    fn outcomes_and_errors_map_onto_the_neutral_port() {
        let mapped = outcome(ChatRetrievalOutcome {
            exchanges: 1,
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
        });
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
