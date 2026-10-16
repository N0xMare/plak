//! The neutral retrieval port over the OpenAI API, Codex, xAI API and
//! SuperGrok side-request executors.

use std::future::Future;
use std::sync::Arc;

use plakous_tools::{
    ExecutionOperationControl, RetrievalError, RetrievalErrorKind, RetrievalFuture, RetrievalKind,
    RetrievalOutcome, RetrievalPort, RetrievalRequest, RetrievalSource, RetrievalUsage,
};

#[cfg(any(feature = "openai", feature = "native-subscriptions"))]
use plakous_openai as responses;
#[cfg(all(
    feature = "xai",
    not(any(feature = "openai", feature = "native-subscriptions"))
))]
use plakous_xai as responses;
use responses::{
    ResponsesRetrievalError, ResponsesRetrievalErrorKind, ResponsesRetrievalOutcome,
    ResponsesRetrievalQuery,
};

/// One session's OpenAI API hosted web search as a [`RetrievalPort`].
#[cfg(feature = "openai")]
pub struct OpenAiRetrievalPort(Arc<plakous_openai::OpenAiRetrieval>);

#[cfg(feature = "openai")]
impl OpenAiRetrievalPort {
    /// Wraps the session's side-request executor.
    #[must_use]
    pub fn new(retrieval: plakous_openai::OpenAiRetrieval) -> Self {
        Self(Arc::new(retrieval))
    }
}

#[cfg(feature = "openai")]
impl RetrievalPort for OpenAiRetrievalPort {
    fn kinds(&self) -> &[RetrievalKind] {
        &[RetrievalKind::WebSearch]
    }

    fn retrieve(
        &self,
        request: RetrievalRequest,
        control: ExecutionOperationControl,
    ) -> RetrievalFuture {
        let retrieval = Arc::clone(&self.0);
        run(self.kinds(), request, move |query| async move {
            retrieval.retrieve(query, control.cancellation()).await
        })
    }
}

/// One session's Codex subscription hosted web search as a [`RetrievalPort`].
#[cfg(feature = "native-subscriptions")]
pub struct CodexRetrievalPort(Arc<plakous_openai::CodexRetrieval>);

#[cfg(feature = "native-subscriptions")]
impl CodexRetrievalPort {
    /// Wraps the session's side-request executor.
    #[must_use]
    pub fn new(retrieval: plakous_openai::CodexRetrieval) -> Self {
        Self(Arc::new(retrieval))
    }
}

#[cfg(feature = "native-subscriptions")]
impl RetrievalPort for CodexRetrievalPort {
    fn kinds(&self) -> &[RetrievalKind] {
        &[RetrievalKind::WebSearch]
    }

    fn retrieve(
        &self,
        request: RetrievalRequest,
        control: ExecutionOperationControl,
    ) -> RetrievalFuture {
        let retrieval = Arc::clone(&self.0);
        run(self.kinds(), request, move |query| async move {
            retrieval.retrieve(query, control.cancellation()).await
        })
    }
}

/// One session's xAI API or SuperGrok hosted web and X search as a
/// [`RetrievalPort`].
#[cfg(any(feature = "xai", feature = "native-subscriptions"))]
pub struct XaiRetrievalPort(Arc<plakous_xai::XaiRetrieval>);

#[cfg(any(feature = "xai", feature = "native-subscriptions"))]
impl XaiRetrievalPort {
    /// Wraps the session's side-request executor.
    #[must_use]
    pub fn new(retrieval: plakous_xai::XaiRetrieval) -> Self {
        Self(Arc::new(retrieval))
    }
}

#[cfg(any(feature = "xai", feature = "native-subscriptions"))]
impl RetrievalPort for XaiRetrievalPort {
    fn kinds(&self) -> &[RetrievalKind] {
        &[RetrievalKind::WebSearch, RetrievalKind::PostSearch]
    }

    fn retrieve(
        &self,
        request: RetrievalRequest,
        control: ExecutionOperationControl,
    ) -> RetrievalFuture {
        let retrieval = Arc::clone(&self.0);
        run(self.kinds(), request, move |query| async move {
            retrieval.retrieve(query, control.cancellation()).await
        })
    }
}

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

fn outcome(outcome: ResponsesRetrievalOutcome) -> RetrievalOutcome {
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

fn error_of(error: &ResponsesRetrievalError) -> RetrievalError {
    let kind = match error.kind() {
        ResponsesRetrievalErrorKind::InvalidRequest => RetrievalErrorKind::InvalidRequest,
        ResponsesRetrievalErrorKind::Unauthorized => RetrievalErrorKind::Unauthorized,
        ResponsesRetrievalErrorKind::Rejected => RetrievalErrorKind::Rejected,
        ResponsesRetrievalErrorKind::Cancelled => RetrievalErrorKind::Cancelled,
        ResponsesRetrievalErrorKind::InvalidResponse => RetrievalErrorKind::InvalidResponse,
        _ => RetrievalErrorKind::Unavailable,
    };
    RetrievalError::new(kind, error.message())
}

#[cfg(test)]
mod tests {
    use super::*;

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
