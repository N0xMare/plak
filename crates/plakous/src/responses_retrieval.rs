//! The neutral retrieval port over the OpenAI API, Codex, xAI API and
//! SuperGrok side-request executors.

use crate::retrieval_outcome::{error_of, outcome};
use std::future::Future;
use std::sync::Arc;

use plakous_tools::{
    ExecutionOperationControl, RetrievalError, RetrievalErrorKind, RetrievalFuture, RetrievalKind,
    RetrievalPort, RetrievalRequest,
};

#[cfg(any(feature = "openai", feature = "codex"))]
use plakous_openai as responses;
#[cfg(all(
    any(feature = "xai", feature = "grok"),
    not(any(feature = "openai", feature = "codex"))
))]
use plakous_xai as responses;
use responses::{ResponsesRetrievalError, ResponsesRetrievalOutcome, ResponsesRetrievalQuery};

#[cfg(feature = "openai")]
mod openai;
#[cfg(feature = "openai")]
pub use openai::OpenAiRetrievalPort;

#[cfg(feature = "codex")]
mod codex;
#[cfg(feature = "codex")]
pub use codex::CodexRetrievalPort;

#[cfg(any(feature = "xai", feature = "grok"))]
mod xai;
#[cfg(any(feature = "xai", feature = "grok"))]
pub use xai::XaiRetrievalPort;

fn run<F, Fut>(kinds: &[RetrievalKind], request: RetrievalRequest, retrieve: F) -> RetrievalFuture
where
    F: FnOnce(ResponsesRetrievalQuery) -> Fut + Send + 'static,
    Fut: Future<Output = Result<ResponsesRetrievalOutcome, ResponsesRetrievalError>> + Send,
{
    let served = kinds.contains(&request.kind());
    Box::pin(async move {
        if !served {
            return Err(RetrievalError::new(
                RetrievalErrorKind::Unsupported,
                "this route does not serve this retrieval",
            ));
        }
        let query = query(request)?;
        retrieve(query)
            .await
            .map(outcome)
            .map_err(|error| error_of(&error))
    })
}

fn query(request: RetrievalRequest) -> Result<ResponsesRetrievalQuery, RetrievalError> {
    match request {
        RetrievalRequest::WebSearch {
            query,
            allowed_domains,
            blocked_domains,
            ..
        } => Ok(ResponsesRetrievalQuery::WebSearch {
            query,
            allowed_domains,
            blocked_domains,
        }),
        RetrievalRequest::PostSearch {
            query,
            handles,
            from_date,
            to_date,
        } => Ok(ResponsesRetrievalQuery::PostSearch {
            query,
            handles,
            from_date,
            to_date,
        }),
        RetrievalRequest::PageFetch { .. } => Err(RetrievalError::new(
            RetrievalErrorKind::Unsupported,
            "this route does not fetch pages",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use responses::ResponsesRetrievalErrorKind;

    #[tokio::test]
    async fn requests_and_errors_map_onto_the_side_request() {
        assert_eq!(
            query(RetrievalRequest::PostSearch {
                query: "q".to_owned(),
                handles: vec!["h".to_owned()],
                from_date: None,
                to_date: Some("2026-10-14".to_owned()),
            }),
            Ok(ResponsesRetrievalQuery::PostSearch {
                query: "q".to_owned(),
                handles: vec!["h".to_owned()],
                from_date: None,
                to_date: Some("2026-10-14".to_owned()),
            })
        );
        let fetch = RetrievalRequest::PageFetch {
            url: "https://example.com/".to_owned(),
            prompt: None,
        };
        assert_eq!(
            query(fetch.clone()).map_err(|error| error.kind()),
            Err(RetrievalErrorKind::Unsupported)
        );
        let refused = run(&[RetrievalKind::WebSearch], fetch, |_| async {
            Ok(ResponsesRetrievalOutcome::default())
        })
        .await;
        assert_eq!(
            refused.map_err(|error| error.kind()),
            Err(RetrievalErrorKind::Unsupported)
        );
        let mapped = error_of(&ResponsesRetrievalError::new(
            ResponsesRetrievalErrorKind::Unauthorized,
            "side request authority is not current",
        ));
        assert_eq!(mapped.kind(), RetrievalErrorKind::Unauthorized);
        assert_eq!(mapped.message(), "side request authority is not current");
    }
}
