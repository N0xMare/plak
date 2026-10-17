//! Hosted web and X search on the xAI API and SuperGrok routes.
//!
//! [`XaiRetrieval`] runs one isolated Responses side request per call on
//! the session's own executor: the same route, key grant or OAuth login,
//! model and client. Before every physical request it revalidates admission
//! exactly as before a model round; it never falls back to another route or
//! account.

use std::fmt;
use std::sync::Arc;

use plakous_http::Client;
use plakous_model::CancellationToken;
#[cfg(feature = "native-auth")]
use plakous_responses_dialect::ResponsesRetrievalDispatch;
use plakous_responses_dialect::{
    ResponsesRetrievalAuthorization, ResponsesRetrievalError, ResponsesRetrievalErrorKind,
    ResponsesRetrievalFlavor, ResponsesRetrievalLimits, ResponsesRetrievalOutcome,
    ResponsesRetrievalQuery, ResponsesRetrievalRequest, ResponsesRetrievalTransport,
    run_responses_retrieval,
};

#[cfg(feature = "native-auth")]
use crate::XaiLanguageModel;
use crate::XaiPlatformModel;

enum Executor {
    Api(Arc<XaiPlatformModel>),
    #[cfg(feature = "native-auth")]
    SuperGrok(Arc<XaiLanguageModel>),
}

/// One session's hosted web and X search on the xAI API or SuperGrok route.
pub struct XaiRetrieval {
    executor: Executor,
    effort: Option<&'static str>,
    limits: ResponsesRetrievalLimits,
}

impl XaiRetrieval {
    pub(crate) fn new(model: Arc<XaiPlatformModel>, effort: Option<&'static str>) -> Self {
        Self {
            executor: Executor::Api(model),
            effort,
            limits: ResponsesRetrievalLimits::default(),
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn supergrok(model: Arc<XaiLanguageModel>, effort: Option<&'static str>) -> Self {
        Self {
            executor: Executor::SuperGrok(model),
            effort,
            limits: ResponsesRetrievalLimits::default(),
        }
    }

    fn client(&self) -> &Client {
        match &self.executor {
            Executor::Api(model) => model.client(),
            #[cfg(feature = "native-auth")]
            Executor::SuperGrok(model) => model.client(),
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
        let model = match &self.executor {
            Executor::Api(model) => model.requested_model(),
            #[cfg(feature = "native-auth")]
            Executor::SuperGrok(model) => model.descriptor().requested_model().as_str(),
        };
        ResponsesRetrievalRequest::new(model, ResponsesRetrievalFlavor::Xai, query)
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
        run_responses_retrieval(self.client(), self, &request, cancellation, self.limits).await
    }
}

impl ResponsesRetrievalTransport for XaiRetrieval {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a> {
        Box::pin(async move {
            let dispatch = match &self.executor {
                Executor::Api(model) => model.side_request(body).await.map(Into::into),
                #[cfg(feature = "native-auth")]
                Executor::SuperGrok(model) => {
                    model.side_request(body).await.map(|(request, guard)| {
                        ResponsesRetrievalDispatch::new(request).with_guard(guard)
                    })
                }
            };
            dispatch.map_err(|message| {
                ResponsesRetrievalError::new(
                    ResponsesRetrievalErrorKind::Unauthorized,
                    format!("side request authority is not current: {message}"),
                )
            })
        })
    }

    fn credential_rejected(&self) -> plakous_responses_dialect::ResponsesCredentialRefresh<'_> {
        match &self.executor {
            Executor::Api(_) => Box::pin(std::future::ready(false)),
            #[cfg(feature = "native-auth")]
            Executor::SuperGrok(model) => Box::pin(model.side_request_rejected()),
        }
    }
}

impl fmt::Debug for XaiRetrieval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XaiRetrieval")
            .field(
                "route",
                &match self.executor {
                    Executor::Api(_) => "xai-api",
                    #[cfg(feature = "native-auth")]
                    Executor::SuperGrok(_) => "grok",
                },
            )
            .field("effort", &self.effort)
            .finish_non_exhaustive()
    }
}
