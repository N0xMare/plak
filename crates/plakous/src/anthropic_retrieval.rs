//! The neutral retrieval port over the Anthropic API and Claude OAuth
//! side-request executor.

use crate::retrieval_outcome::{error_of, outcome};
use std::sync::Arc;

use plakous_anthropic::{AnthropicRetrieval, MessagesRetrievalQuery};
use plakous_tools::{
    OperationContext, RetrievalError, RetrievalErrorKind, RetrievalFuture, RetrievalKind,
    RetrievalPort, RetrievalRequest,
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

    fn retrieve(&self, request: RetrievalRequest, control: OperationContext) -> RetrievalFuture {
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
            max_uses,
        } => Ok(MessagesRetrievalQuery::WebSearch {
            query,
            allowed_domains,
            blocked_domains,
            max_uses,
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

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_anthropic::{MessagesRetrievalError, MessagesRetrievalErrorKind};

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
