//! Image generation and editing on the xAI API route.
//!
//! [`XaiImages`] sends one `/v1/images/*` request per call on the session's
//! own executor: the same route, key grant and client. Before every
//! physical request it revalidates admission exactly as before a model
//! round; it never falls back to another route or account.

use std::fmt;
use std::sync::Arc;

use plakous_model::CancellationToken;
use plakous_responses_dialect::{
    RESPONSES_IMAGE_EXCHANGE_TIMEOUT, ResponsesImageAuthorization, ResponsesImageEndpoint,
    ResponsesImageFlavor, ResponsesImageRequest, ResponsesImageTransport, ResponsesRetrievalError,
    ResponsesRetrievalErrorKind, ResponsesRetrievalLimits, run_responses_image,
};

use crate::XaiPlatformModel;

/// The image model the xAI API route generates with.
pub const XAI_IMAGE_MODEL: &str = "grok-imagine-image-2.0";
/// The xAI API generation path.
pub const XAI_IMAGE_GENERATIONS_PATH: &str = "/v1/images/generations";
/// The xAI API edit path.
pub const XAI_IMAGE_EDITS_PATH: &str = "/v1/images/edits";
/// One session's image generation on the xAI API route.
pub struct XaiImages {
    model: Arc<XaiPlatformModel>,
    limits: ResponsesRetrievalLimits,
}

impl XaiImages {
    pub(crate) fn new(model: Arc<XaiPlatformModel>) -> Self {
        Self {
            model,
            limits: ResponsesRetrievalLimits {
                exchange_timeout: RESPONSES_IMAGE_EXCHANGE_TIMEOUT,
                jitter: None,
            },
        }
    }

    /// Replaces the exchange bounds and backoff sample.
    #[must_use]
    pub const fn with_limits(mut self, limits: ResponsesRetrievalLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The exact request for `prompt`.
    #[must_use]
    pub fn request(&self, prompt: impl Into<String>) -> ResponsesImageRequest {
        ResponsesImageRequest::new(XAI_IMAGE_MODEL, ResponsesImageFlavor::Xai, prompt)
    }

    /// Runs one images request and returns the image bytes.
    ///
    /// # Errors
    ///
    /// Returns the request's failure after its bounded retries.
    pub async fn generate(
        &self,
        request: &ResponsesImageRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, ResponsesRetrievalError> {
        run_responses_image(
            self.model.client(),
            self,
            request,
            cancellation,
            self.limits,
        )
        .await
    }
}

impl ResponsesImageTransport for XaiImages {
    fn authorize<'a>(
        &'a self,
        endpoint: ResponsesImageEndpoint,
        body: &'a [u8],
    ) -> ResponsesImageAuthorization<'a> {
        let path = match endpoint {
            ResponsesImageEndpoint::Generations => XAI_IMAGE_GENERATIONS_PATH,
            ResponsesImageEndpoint::Edits => XAI_IMAGE_EDITS_PATH,
        };
        Box::pin(async move {
            self.model
                .side_request_to(path, "application/json", body)
                .await
                .map(Into::into)
                .map_err(|message| {
                    ResponsesRetrievalError::new(
                        ResponsesRetrievalErrorKind::Unauthorized,
                        format!("images request authority is not current: {message}"),
                    )
                })
        })
    }
}

impl fmt::Debug for XaiImages {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XaiImages")
            .field("route", &"xai-api")
            .field("model", &XAI_IMAGE_MODEL)
            .finish_non_exhaustive()
    }
}
