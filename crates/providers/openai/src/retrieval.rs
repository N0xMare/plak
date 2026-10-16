//! Hosted web search on the OpenAI API route.
//!
//! [`OpenAiRetrieval`] runs one isolated Responses side request per call on
//! the session's own executor: the same route, key generation, project,
//! model and client. Before every physical request it revalidates admission
//! exactly as before a model round; it never falls back to another route or
//! account.

use std::fmt;
use std::sync::Arc;

use plakous_model::CancellationToken;
use plakous_responses_dialect::{
    ResponsesRetrievalAuthorization, ResponsesRetrievalError, ResponsesRetrievalErrorKind,
    ResponsesRetrievalFlavor, ResponsesRetrievalLimits, ResponsesRetrievalOutcome,
    ResponsesRetrievalQuery, ResponsesRetrievalRequest, ResponsesRetrievalTransport,
    run_responses_retrieval,
};

use crate::OpenAiPlatformModel;

/// One session's hosted web search on the OpenAI API route.
pub struct OpenAiRetrieval {
    model: Arc<OpenAiPlatformModel>,
    effort: Option<&'static str>,
    limits: ResponsesRetrievalLimits,
}

impl OpenAiRetrieval {
    pub(crate) fn new(model: Arc<OpenAiPlatformModel>, effort: Option<&'static str>) -> Self {
        Self {
            model,
            effort,
            limits: ResponsesRetrievalLimits::default(),
        }
    }

    /// Replaces the exchange bounds and backoff sample.
    #[must_use]
    pub const fn with_limits(mut self, limits: ResponsesRetrievalLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The exact side request for `query`.
    #[must_use]
    pub fn request(&self, query: ResponsesRetrievalQuery) -> ResponsesRetrievalRequest {
        ResponsesRetrievalRequest::new(
            self.model.requested_model(),
            ResponsesRetrievalFlavor::OpenAi,
            query,
        )
        .with_effort(self.effort)
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
        let request = self.request(query);
        run_responses_retrieval(
            self.model.client(),
            self,
            &request,
            cancellation,
            self.limits,
        )
        .await
    }
}

impl ResponsesRetrievalTransport for OpenAiRetrieval {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a> {
        Box::pin(async move {
            self.model.side_request(body).await.map_err(|message| {
                ResponsesRetrievalError::new(
                    ResponsesRetrievalErrorKind::Unauthorized,
                    format!("side request authority is not current: {message}"),
                )
            })
        })
    }
}

impl fmt::Debug for OpenAiRetrieval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiRetrieval")
            .field("route", &"openai")
            .field("effort", &self.effort)
            .finish_non_exhaustive()
    }
}
