//! Transport-free image generation port and request vocabulary.

use plakous_tools::{OperationContext, RetrievalError};
use std::{future::Future, pin::Pin};

/// Longest accepted prompt, in bytes.
pub const MAX_IMAGE_PROMPT_BYTES: usize = 4000;
/// Most edit inputs any route accepts.
pub const MAX_IMAGE_EDIT_INPUTS: usize = 16;
/// Largest edit input, in bytes.
pub const MAX_IMAGE_EDIT_INPUT_BYTES: u64 = plakous_tools::MAX_WORKSPACE_READ_SLICE_BYTES;

/// One generation, or an edit when it names input images.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageGenRequest {
    /// What to draw, or how to change the inputs.
    pub prompt: String,
    /// `WIDTHxHEIGHT` or `auto`; `None` leaves the provider's default.
    pub size: Option<String>,
    /// Input images of an edit, in the order named.
    pub edit_from: Vec<ImageGenSource>,
}

/// One edit input read from the workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageGenSource {
    /// `image/png`, `image/jpeg` or `image/webp`.
    pub media_type: &'static str,
    /// The file's bytes.
    pub bytes: Vec<u8>,
}

/// The future one generation returns: the image bytes, or the request's
/// failure.
pub type ImageGenFuture =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, RetrievalError>> + Send + 'static>>;

/// One route's image generation, bound to the session's exact route,
/// account and credential.
///
/// An implementation revalidates admission immediately before each request,
/// as before a model round, and never falls back to another route or
/// account.
pub trait ImageGenPort: Send + Sync {
    /// Most input images one edit may name.
    fn max_edit_images(&self) -> usize;

    /// Whether the route takes a requested size.
    fn takes_size(&self) -> bool;

    /// Runs one generation or edit as one request.
    fn generate(&self, request: ImageGenRequest, control: OperationContext) -> ImageGenFuture;
}
