//! One images-endpoint request for image generation or editing.
//!
//! OpenAI Platform, Codex and xAI expose OpenAI-shaped
//! `images/generations` and `images/edits` endpoints. One `image_gen` tool
//! call is one logical request with a JSON body; its response carries the
//! image as base64. Retries happen only where no image can have been made:
//! before the request was transmitted, on a rate-limit refusal, and, when
//! the transport opts in, on the Codex wrapped-503 401.
//! A request the provider may have accepted is never sent again, so a
//! generation is never paid twice.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use plakous_exchange::side_request::{self, BodyCollector, Broken, Document, ResponseReader};
use plakous_http::Client;
use plakous_model::CancellationToken;
use serde_json::{Map, Value, json};

use crate::retrieval::{
    ResponsesCredentialRefresh, ResponsesRetrievalAuthorization, ResponsesRetrievalDispatch,
    ResponsesRetrievalError, ResponsesRetrievalErrorKind, ResponsesRetrievalLimits,
    retried_wrapped_overload, status_error, wrapped_overload_status_error,
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
pub type ResponsesImageAuthorization<'a> = Pin<
    Box<
        dyn Future<Output = Result<ResponsesRetrievalDispatch, ResponsesRetrievalError>>
            + Send
            + 'a,
    >,
>;

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

    /// See [`crate::ResponsesRetrievalTransport::credential_rejected`]: one
    /// claimed refresh after a 401, then one more request.
    fn credential_rejected(&self) -> ResponsesCredentialRefresh<'_> {
        Box::pin(std::future::ready(false))
    }

    /// See [`crate::ResponsesRetrievalTransport::retry_wrapped_overload`].
    fn retry_wrapped_overload(&self) -> bool {
        false
    }

    /// See [`crate::ResponsesRetrievalTransport::contract_refusal`].
    fn contract_refusal(&self, status: u16, body: &[u8]) -> Option<String> {
        let _ = (status, body);
        None
    }
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
/// Returns the request's failure; only an untransmitted request, a
/// rate-limit refusal, and an opted-in wrapped-503 401 are retried, under
/// [`plakous_attempt::RetryPolicy::TURN`].
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
    let reader = ImageReader {
        wrapped_overload: transport.retry_wrapped_overload(),
    };
    let transport = ImageTransport {
        inner: transport,
        endpoint,
    };
    let response =
        side_request::exchange_with_retry(client, &transport, &body, reader, cancellation, limits)
            .await?;
    decode_responses_image(&response)
}

struct ImageTransport<'a> {
    inner: &'a dyn ResponsesImageTransport,
    endpoint: ResponsesImageEndpoint,
}
impl side_request::RetrievalTransport for ImageTransport<'_> {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a> {
        self.inner.authorize(self.endpoint, body)
    }
    fn credential_rejected(&self) -> ResponsesCredentialRefresh<'_> {
        self.inner.credential_rejected()
    }
    fn retry_wrapped_overload(&self) -> bool {
        self.inner.retry_wrapped_overload()
    }
    fn contract_refusal(&self, status: u16, body: &[u8]) -> Option<String> {
        self.inner.contract_refusal(status, body)
    }
}
struct ImageReader {
    wrapped_overload: bool,
}
struct ImageBody(Document);
impl ResponseReader for ImageReader {
    type Collector = ImageBody;
    fn start(&self, _: &plakous_http::Response) -> ImageBody {
        ImageBody(Document::new(
            RESPONSES_IMAGE_MAX_RESPONSE_BYTES,
            "the images response exceeds 48 MiB",
        ))
    }
    fn status_error(&self, status: u16, body: &[u8]) -> ResponsesRetrievalError {
        if retried_wrapped_overload(self.wrapped_overload, status, body) {
            wrapped_overload_status_error(body)
        } else {
            status_error(status, body)
        }
    }
    fn retried_as(&self, status: u16, body: &[u8]) -> Option<u16> {
        retried_wrapped_overload(self.wrapped_overload, status, body).then_some(503)
    }
    fn policy(&self) -> side_request::SideRequestPolicy {
        side_request::SideRequestPolicy::Image
    }
}
impl BodyCollector for ImageBody {
    type Output = Vec<u8>;
    fn push(&mut self, bytes: &[u8]) -> Result<(), Broken> {
        self.0.push(bytes)
    }
    fn finish(self) -> Result<Vec<u8>, Broken> {
        self.0.finish()
    }
    fn transport_failure(self, failure: plakous_http::Failure) -> Result<Vec<u8>, Broken> {
        if failure.kind() == plakous_http::ErrorKind::Internal {
            Ok(Vec::new())
        } else {
            self.0.finish()
        }
    }
}

fn invalid_request(message: &str) -> ResponsesRetrievalError {
    ResponsesRetrievalError::new(ResponsesRetrievalErrorKind::InvalidRequest, message)
}

fn invalid_response(message: &str) -> ResponsesRetrievalError {
    ResponsesRetrievalError::new(ResponsesRetrievalErrorKind::InvalidResponse, message)
}

#[cfg(test)]
mod tests;
