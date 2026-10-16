//! One images-endpoint request for image generation or editing.
//!
//! OpenAI Platform, Codex and xAI expose OpenAI-shaped
//! `images/generations` and `images/edits` endpoints. One `image_gen` tool
//! call is one logical request with a JSON body; its response carries the
//! image as base64. Retries happen only where no image can have been made:
//! before the request was transmitted, and on a rate-limit refusal. A
//! request that may have reached the provider is never sent again, so a
//! generation is never paid twice.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use plakous_attempt::{
    FailurePoint, Jitter, ObservationSnapshot, ProviderAcceptance, ReplaySafety, RetryPolicy,
    TerminalFacts,
};
use plakous_exchange::{cancellable, read_bounded_body};
use plakous_http::{
    CancellationToken as HttpCancellationToken, Client, ExchangeControl, Request as HttpRequest,
};
use plakous_model::CancellationToken;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::retrieval::{
    MAX_ERROR_BODY_BYTES, ResponsesRetrievalError, ResponsesRetrievalErrorKind,
    ResponsesRetrievalLimits, cancelled, status_error, transport_point, unavailable,
};

/// Largest response body read, in bytes: a base64 image inside JSON.
pub const RESPONSES_IMAGE_MAX_RESPONSE_BYTES: usize = 48 * 1024 * 1024;
/// Largest decoded image accepted, in bytes.
pub const RESPONSES_IMAGE_MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Exchange bound of one images request: generation is slow.
pub const RESPONSES_IMAGE_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(300);

/// The route family whose images endpoint shape a request uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesImageFlavor {
    /// OpenAI Platform `/v1/images/*`: the image always returns as base64;
    /// edits name their inputs in `images[].image_url`.
    OpenAi,
    /// Codex `<backend base>/images/*`: the OpenAI body, at most five edit
    /// inputs.
    Codex,
    /// xAI API `/v1/images/*`: `response_format: b64_json`, one edit input
    /// in `image`, no size.
    Xai,
}

impl ResponsesImageFlavor {
    /// Most input images one edit may name.
    #[must_use]
    pub const fn max_edit_images(self) -> usize {
        match self {
            Self::OpenAi => 16,
            Self::Codex => 5,
            Self::Xai => 1,
        }
    }

    /// Whether the endpoint takes a `size`.
    #[must_use]
    pub const fn takes_size(self) -> bool {
        matches!(self, Self::OpenAi | Self::Codex)
    }
}

/// One input image of an edit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesImageSource {
    /// `image/png`, `image/jpeg` or `image/webp`.
    pub media_type: String,
    /// The file's bytes.
    pub bytes: Vec<u8>,
}

impl ResponsesImageSource {
    fn data_url(&self) -> String {
        format!(
            "data:{};base64,{}",
            self.media_type,
            STANDARD.encode(&self.bytes)
        )
    }
}

/// One generation, or one edit when it names input images.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesImageRequest {
    model: String,
    flavor: ResponsesImageFlavor,
    prompt: String,
    size: Option<String>,
    edit_from: Vec<ResponsesImageSource>,
}

impl ResponsesImageRequest {
    /// A generation of `prompt` with the route's image `model`.
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        flavor: ResponsesImageFlavor,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            flavor,
            prompt: prompt.into(),
            size: None,
            edit_from: Vec::new(),
        }
    }

    /// This request at `size` (`WIDTHxHEIGHT` or `auto`).
    #[must_use]
    pub fn with_size(mut self, size: Option<String>) -> Self {
        self.size = size;
        self
    }

    /// This request as an edit of `images`.
    #[must_use]
    pub fn with_edit_from(mut self, images: Vec<ResponsesImageSource>) -> Self {
        self.edit_from = images;
        self
    }

    /// Whether the request edits input images.
    #[must_use]
    pub fn is_edit(&self) -> bool {
        !self.edit_from.is_empty()
    }

    /// The exact body.
    ///
    /// # Errors
    ///
    /// Refuses a size the route takes none of and more input images than
    /// the route accepts.
    pub fn body(&self) -> Result<Vec<u8>, ResponsesRetrievalError> {
        if self.size.is_some() && !self.flavor.takes_size() {
            return Err(invalid_request(
                "this route's images endpoint takes no size",
            ));
        }
        if self.edit_from.len() > self.flavor.max_edit_images() {
            return Err(ResponsesRetrievalError::new(
                ResponsesRetrievalErrorKind::InvalidRequest,
                format!(
                    "this route edits at most {} input images",
                    self.flavor.max_edit_images()
                ),
            ));
        }
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(self.model));
        body.insert("prompt".to_owned(), json!(self.prompt));
        body.insert("n".to_owned(), json!(1));
        if let Some(size) = &self.size {
            body.insert("size".to_owned(), json!(size));
        }
        match self.flavor {
            ResponsesImageFlavor::OpenAi | ResponsesImageFlavor::Codex => {
                if self.is_edit() {
                    let images = self
                        .edit_from
                        .iter()
                        .map(|image| json!({"image_url": image.data_url()}))
                        .collect::<Vec<_>>();
                    body.insert("images".to_owned(), Value::Array(images));
                }
            }
            ResponsesImageFlavor::Xai => {
                body.insert("response_format".to_owned(), json!("b64_json"));
                if let Some(image) = self.edit_from.first() {
                    body.insert(
                        "image".to_owned(),
                        json!({"type": "image_url", "url": image.data_url()}),
                    );
                }
            }
        }
        serde_json::to_vec(&Value::Object(body))
            .map_err(|_| invalid_request("images request could not be encoded"))
    }
}

/// Which images endpoint a request goes to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesImageEndpoint {
    /// `images/generations`.
    Generations,
    /// `images/edits`.
    Edits,
}

/// The future [`ResponsesImageTransport::authorize`] returns.
pub type ResponsesImageAuthorization<'a> =
    Pin<Box<dyn Future<Output = Result<HttpRequest, ResponsesRetrievalError>> + Send + 'a>>;

/// The route's half of an images request.
pub trait ResponsesImageTransport: Send + Sync {
    /// Revalidates admission and the credential, then builds the exact
    /// authorized request carrying `body` to `endpoint`. Called before
    /// every physical request, retries included.
    fn authorize<'a>(
        &'a self,
        endpoint: ResponsesImageEndpoint,
        body: &'a [u8],
    ) -> ResponsesImageAuthorization<'a>;
}

/// The image bytes of a successful images response.
///
/// # Errors
///
/// Refuses a body without exactly one inline base64 image, a URL-only
/// image (requests ask for inline bytes), and an image over
/// [`RESPONSES_IMAGE_MAX_IMAGE_BYTES`].
pub fn decode_responses_image(body: &[u8]) -> Result<Vec<u8>, ResponsesRetrievalError> {
    let value = serde_json::from_slice::<Value>(body)
        .map_err(|_| invalid_response("the images response is not JSON"))?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("the images response has no data"))?;
    let [image] = data.as_slice() else {
        return Err(invalid_response(
            "the images response does not carry exactly one image",
        ));
    };
    if let Some(encoded) = image.get("b64_json").and_then(Value::as_str) {
        return decode_base64(encoded);
    }
    match image.get("url").and_then(Value::as_str) {
        Some(url) => match url
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(";base64,"))
        {
            Some((_, encoded)) => decode_base64(encoded),
            None => Err(invalid_response(
                "the provider returned an image URL instead of the image bytes",
            )),
        },
        None => Err(invalid_response("the images response carries no image")),
    }
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, ResponsesRetrievalError> {
    if encoded.len() / 4 * 3 > RESPONSES_IMAGE_MAX_IMAGE_BYTES {
        return Err(invalid_response("the generated image exceeds 16 MiB"));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| invalid_response("the generated image is not valid base64"))?;
    if bytes.is_empty() {
        return Err(invalid_response("the generated image is empty"));
    }
    Ok(bytes)
}

/// Runs one images request to its image bytes.
///
/// # Errors
///
/// Returns the request's failure; only an untransmitted request and a
/// rate-limit refusal are retried, under [`RetryPolicy::TURN`].
pub async fn run_responses_image(
    client: &Client,
    transport: &dyn ResponsesImageTransport,
    request: &ResponsesImageRequest,
    cancellation: &CancellationToken,
    limits: ResponsesRetrievalLimits,
) -> Result<Vec<u8>, ResponsesRetrievalError> {
    let body = request.body()?;
    let endpoint = if request.is_edit() {
        ResponsesImageEndpoint::Edits
    } else {
        ResponsesImageEndpoint::Generations
    };
    let mut physical = 0_u32;
    let mut waited = Duration::ZERO;
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        physical = physical.saturating_add(1);
        let (point, retry_after, mut error) =
            match exchange_once(client, transport, endpoint, &body, cancellation, limits).await {
                Ok(Attempted::Done(response)) => return decode_responses_image(&response),
                Ok(Attempted::Retry {
                    point,
                    retry_after,
                    error,
                }) => (point, retry_after, error),
                Err(mut error) => {
                    error.physical_requests = physical;
                    return Err(error);
                }
            };
        let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: physical,
            provider_acceptance: ProviderAcceptance::NotAccepted,
            visible_output: false,
            replay_safety: ReplaySafety::CostOnly,
            hosted_effects_possible: false,
            logical_success: false,
        });
        let jitter = limits.jitter.unwrap_or_else(Jitter::random);
        let Some(delay) = RetryPolicy::TURN.next_delay(facts, point, retry_after, waited, jitter)
        else {
            error.physical_requests = physical;
            return Err(error);
        };
        waited = waited.saturating_add(delay);
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancellation.clone().cancelled() => return Err(cancelled()),
        }
    }
}

enum Attempted {
    Done(Vec<u8>),
    Retry {
        point: FailurePoint,
        retry_after: Option<Duration>,
        error: ResponsesRetrievalError,
    },
}

async fn exchange_once(
    client: &Client,
    transport: &dyn ResponsesImageTransport,
    endpoint: ResponsesImageEndpoint,
    body: &[u8],
    cancellation: &CancellationToken,
    limits: ResponsesRetrievalLimits,
) -> Result<Attempted, ResponsesRetrievalError> {
    let request = transport.authorize(endpoint, body).await?;
    let http_cancellation = HttpCancellationToken::new();
    let started = Instant::now();
    let deadline = started
        .checked_add(limits.exchange_timeout)
        .unwrap_or(started);
    let exchanged = cancellable(
        client.execute(
            request,
            ExchangeControl::streaming(deadline.into_std(), http_cancellation.clone())
                .with_body_deadline(Some(deadline.into_std())),
        ),
        cancellation,
        &http_cancellation,
    )
    .await;
    if exchanged.cancelled {
        return Err(cancelled());
    }
    let response = match exchanged.value {
        Ok(response) => response,
        Err(failure) => {
            let point = transport_point(failure);
            let error = unavailable(failure);
            // A request that may have reached the provider may have made
            // the image: it is not sent again.
            if matches!(point, FailurePoint::AfterSend | FailurePoint::MidStream) {
                return Err(error);
            }
            return Ok(Attempted::Retry {
                point,
                retry_after: None,
                error,
            });
        }
    };
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(&plakous_http::HeaderName::from_static("retry-after"))
        .and_then(|value| value.to_str().ok())
        .and_then(plakous_attempt::retry_after_seconds);
    let limit = if status == 200 {
        RESPONSES_IMAGE_MAX_RESPONSE_BYTES
    } else {
        MAX_ERROR_BODY_BYTES
    };
    let read = cancellable(
        read_bounded_body(
            response.into_body(),
            cancellation,
            &http_cancellation,
            limit,
        ),
        cancellation,
        &http_cancellation,
    )
    .await;
    if read.cancelled {
        return Err(cancelled());
    }
    let bytes = read.value.unwrap_or_default();
    if status != 200 {
        let mut error = status_error(status, &bytes);
        if status == 429
            && let Some(point) = RetryPolicy::status_failure_point(status)
        {
            return Ok(Attempted::Retry {
                point,
                retry_after,
                error,
            });
        }
        error.status = Some(status);
        return Err(error);
    }
    if bytes.len() > limit {
        return Err(invalid_response("the images response exceeds 48 MiB"));
    }
    Ok(Attempted::Done(bytes))
}

fn invalid_request(message: &str) -> ResponsesRetrievalError {
    ResponsesRetrievalError::new(ResponsesRetrievalErrorKind::InvalidRequest, message)
}

fn invalid_response(message: &str) -> ResponsesRetrievalError {
    ResponsesRetrievalError::new(ResponsesRetrievalErrorKind::InvalidResponse, message)
}

#[cfg(test)]
mod tests;
