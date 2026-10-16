//! Image generation and editing on the OpenAI API route, and the Codex
//! images request shape.
//!
//! [`OpenAiImages`] sends one `/v1/images/*` request per call on the
//! session's own executor: the same route, key generation, project and
//! client. Before every physical request it revalidates admission exactly
//! as before a model round; it never falls back to another route or
//! account. The Codex route's subscription transport is not built, so
//! Codex has no images executor; its paths and model are recorded here.

use std::fmt;
use std::sync::Arc;

use plakous_model::CancellationToken;
use plakous_responses_dialect::{
    RESPONSES_IMAGE_EXCHANGE_TIMEOUT, ResponsesImageAuthorization, ResponsesImageEndpoint,
    ResponsesImageFlavor, ResponsesImageRequest, ResponsesImageTransport, ResponsesRetrievalError,
    ResponsesRetrievalErrorKind, ResponsesRetrievalLimits, run_responses_image,
};

use crate::OpenAiPlatformModel;

/// The image model the OpenAI API route generates with.
pub const OPENAI_IMAGE_MODEL: &str = "gpt-image-2";
/// The OpenAI API generation path.
pub const OPENAI_IMAGE_GENERATIONS_PATH: &str = "/v1/images/generations";
/// The OpenAI API edit path.
pub const OPENAI_IMAGE_EDITS_PATH: &str = "/v1/images/edits";
/// The image model the Codex route generates with.
pub const CODEX_IMAGE_MODEL: &str = "gpt-image-2";
/// The Codex generation path under the backend origin.
pub const CODEX_IMAGE_GENERATIONS_PATH: &str = "/backend-api/codex/images/generations";
/// The Codex edit path under the backend origin.
pub const CODEX_IMAGE_EDITS_PATH: &str = "/backend-api/codex/images/edits";

/// One session's image generation on the OpenAI API route.
pub struct OpenAiImages {
    model: Arc<OpenAiPlatformModel>,
    limits: ResponsesRetrievalLimits,
}

impl OpenAiImages {
    pub(crate) fn new(model: Arc<OpenAiPlatformModel>) -> Self {
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
        ResponsesImageRequest::new(OPENAI_IMAGE_MODEL, ResponsesImageFlavor::OpenAi, prompt)
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

impl ResponsesImageTransport for OpenAiImages {
    fn authorize<'a>(
        &'a self,
        endpoint: ResponsesImageEndpoint,
        body: &'a [u8],
    ) -> ResponsesImageAuthorization<'a> {
        let path = match endpoint {
            ResponsesImageEndpoint::Generations => OPENAI_IMAGE_GENERATIONS_PATH,
            ResponsesImageEndpoint::Edits => OPENAI_IMAGE_EDITS_PATH,
        };
        Box::pin(async move {
            self.model
                .side_request_to(path, "application/json", body)
                .await
                .map_err(|message| {
                    ResponsesRetrievalError::new(
                        ResponsesRetrievalErrorKind::Unauthorized,
                        format!("images request authority is not current: {message}"),
                    )
                })
        })
    }
}

impl fmt::Debug for OpenAiImages {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiImages")
            .field("route", &"openai")
            .field("model", &OPENAI_IMAGE_MODEL)
            .finish_non_exhaustive()
    }
}
