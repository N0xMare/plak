//! `image_gen`: one image generation or edit through the session's route,
//! written into the workspace.
//!
//! A call validates its arguments, refuses an output path that already
//! exists, reads its edit inputs from the workspace, and hands one
//! [`ImageGenRequest`] to the session's [`ImageGenPort`], which sends one
//! images request on the session's own route, account and credential. The
//! returned image is written through the workspace mutation seam, which
//! keeps it contained and never overwrites a file. The tool is mutating: a
//! crash after its claim is an unknown outcome, never replayed, so a
//! generation is never paid for twice by recovery.

use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use plakous_model::{
    ImageBlock, MediaSource, TextBlock, ToolConcurrency, ToolDefinition, ToolEffect,
    ToolInputContract, ToolName, ToolOutputContract, ToolResultPart,
};
use plakous_tools::{
    CodingPresentation, ExecutionBackendErrorKind, ExecutionOperationControl, ExecutionValue,
    ImageValue, MAX_STANDARD_IMAGE_BYTES, MAX_WORKSPACE_READ_SLICE_BYTES, MenuEntry,
    PassthroughPack, PromptPin, RegisteredToolId, RetrievalError, RetrievalErrorKind, ToolContext,
    ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation,
    ToolOutcome, ToolPack, ToolRegistration, ToolRuntimeError, ToolRuntimeErrorKind,
    WorkspaceExpectedFileState, WorkspaceMutation, WorkspaceMutationTransaction,
    WorkspaceReadSliceRequest, WorkspaceReplacement, object_schema, operation_control,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::text::PLAKOUS_WEB_IMAGE_GEN_DESCRIPTION;
use crate::{ISSUER, SEMANTIC_VERSION, WEB_PROMPT_PINS, invalid, registration_error, require_text};

/// Longest accepted prompt, in bytes.
pub const MAX_IMAGE_PROMPT_BYTES: usize = 4000;
/// Most edit inputs any route accepts.
pub const MAX_IMAGE_EDIT_INPUTS: usize = 16;
/// Largest edit input, in bytes.
pub const MAX_IMAGE_EDIT_INPUT_BYTES: u64 = MAX_WORKSPACE_READ_SLICE_BYTES;

const TOOL_NAME: &str = "image_gen";
const MAX_PATH_BYTES: usize = 1024;

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

/// One finished generation with the physical requests it sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageGenOutcome {
    /// The image bytes.
    pub bytes: Vec<u8>,
    /// Physical images requests sent, retries included; `None` when the port
    /// cannot say, which a helper budget charges as the whole declared
    /// maximum.
    pub physical_requests: Option<u32>,
}

/// The future [`ImageGenPort::generate_counted`] returns.
pub type ImageGenOutcomeFuture =
    Pin<Box<dyn Future<Output = Result<ImageGenOutcome, RetrievalError>> + Send + 'static>>;

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
    fn generate(
        &self,
        request: ImageGenRequest,
        control: ExecutionOperationControl,
    ) -> ImageGenFuture;

    /// The most physical requests one generation can send, retries
    /// included; `None` when the port declares no bound. A helper budget
    /// (spec 01 §9.8.7) reserves this before the call.
    fn max_requests(&self) -> Option<u32> {
        None
    }

    /// Runs one generation like [`Self::generate`] and reports the physical
    /// requests it sent. The default reports no count.
    fn generate_counted(
        &self,
        request: ImageGenRequest,
        control: ExecutionOperationControl,
    ) -> ImageGenOutcomeFuture {
        let generated = self.generate(request, control);
        Box::pin(async move {
            generated.await.map(|bytes| ImageGenOutcome {
                bytes,
                physical_requests: None,
            })
        })
    }
}

/// The `image_gen` tool: one registration, appended after the core menu in
/// every vocabulary under the same name.
#[derive(Clone)]
pub struct ImageGenPack {
    port: Arc<dyn ImageGenPort>,
}

impl ImageGenPack {
    /// Binds `image_gen` to the route's `port`.
    #[must_use]
    pub fn new(port: Arc<dyn ImageGenPort>) -> Self {
        Self { port }
    }

    fn registration(&self) -> Result<ToolRegistration, ToolRuntimeError> {
        let definition = ToolDefinition::new(
            ToolName::new(TOOL_NAME).map_err(registration_error("image_gen name is invalid"))?,
            SEMANTIC_VERSION,
            PLAKOUS_WEB_IMAGE_GEN_DESCRIPTION,
            ToolInputContract::structured(schema())
                .map_err(registration_error("image_gen schema is invalid"))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::Mutating,
        )
        .map_err(registration_error("image_gen definition is invalid"))?;
        Ok(ToolRegistration::new(
            RegisteredToolId::new("plakous.web.image_gen.v1")
                .map_err(registration_error("image_gen id is invalid"))?,
            ToolExecutorId::new("plakous.web.image_gen.executor.v1")
                .map_err(registration_error("image_gen executor id is invalid"))?,
            ToolExecutorBindingIdentity::new(1, &[ISSUER, TOOL_NAME.as_bytes()])
                .map_err(registration_error("image_gen binding identity is invalid"))?,
            definition,
            Arc::new(ImageGenTool {
                port: Arc::clone(&self.port),
            }),
        )
        .nested_eligible())
    }
}

impl ImageGenPack {
    /// The SDK-only `image_gen` registration, carrying the port's declared
    /// most physical requests per call.
    pub(crate) fn sdk_registration(&self) -> Result<ToolRegistration, ToolRuntimeError> {
        let tool = plakous_tools::PYTHON_HARNESS_SDK_IMAGE_GEN_V1;
        let max = crate::sdk::declared(self.port.max_requests(), tool.name)?;
        crate::sdk::sdk_registration(
            tool,
            (
                PLAKOUS_WEB_IMAGE_GEN_DESCRIPTION,
                schema(),
                ToolConcurrency::Serial,
                ToolEffect::Mutating,
            ),
            Some(max),
            Arc::new(ImageGenTool {
                port: Arc::clone(&self.port),
            }),
        )
    }
}

impl ToolPack for ImageGenPack {
    fn id(&self) -> &'static str {
        "plakous.web.image_gen.v1"
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        Ok(vec![self.registration()?])
    }

    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let name =
            ToolName::new(TOOL_NAME).map_err(registration_error("image_gen name is invalid"))?;
        PassthroughPack::appended_menu(&[name])
    }

    fn pins(&self) -> &'static [PromptPin] {
        WEB_PROMPT_PINS
    }
}

fn schema() -> Value {
    object_schema(
        json!({
            "prompt": {"type": "string", "minLength": 1, "maxLength": MAX_IMAGE_PROMPT_BYTES},
            "path": {"type": "string", "minLength": 1, "maxLength": MAX_PATH_BYTES},
            "size": {"type": "string", "minLength": 4, "maxLength": 11},
            "edit_from": {
                "type": "array",
                "items": {"type": "string", "minLength": 1, "maxLength": MAX_PATH_BYTES},
                "maxItems": MAX_IMAGE_EDIT_INPUTS
            }
        }),
        &["prompt", "path"],
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    prompt: String,
    path: String,
    size: Option<String>,
    #[serde(default)]
    edit_from: Vec<String>,
}

/// One validated call.
#[derive(Debug)]
struct Call {
    prompt: String,
    /// The output path as the model wrote it, for the result.
    shown: String,
    path: PathBuf,
    size: Option<String>,
    edit_from: Vec<(String, PathBuf)>,
}

impl Call {
    fn parse(
        arguments: Value,
        context: &ToolContext,
        port: &dyn ImageGenPort,
    ) -> Result<Self, ToolRuntimeError> {
        let arguments: Arguments = serde_json::from_value(arguments)
            .map_err(|_| invalid("image_gen arguments are invalid"))?;
        let prompt = require_text(&arguments.prompt, MAX_IMAGE_PROMPT_BYTES, "prompt")?;
        let shown = require_text(&arguments.path, MAX_PATH_BYTES, "path")?;
        let path = context.workspace().relative_input(&shown)?;
        let size = arguments.size.map(|size| size.trim().to_owned());
        if let Some(size) = &size {
            if !valid_size(size) {
                return Err(invalid("size must be WIDTHxHEIGHT or auto"));
            }
            if !port.takes_size() {
                return Err(invalid("size is not available on this route; omit it"));
            }
        }
        if arguments.edit_from.len() > port.max_edit_images() {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                format!(
                    "edit_from names {} images; this route edits at most {}",
                    arguments.edit_from.len(),
                    port.max_edit_images()
                ),
            ));
        }
        let edit_from = arguments
            .edit_from
            .iter()
            .map(|input| {
                let shown = require_text(input, MAX_PATH_BYTES, "edit_from")?;
                let path = context.workspace().relative_input(&shown)?;
                Ok((shown, path))
            })
            .collect::<Result<Vec<_>, ToolRuntimeError>>()?;
        Ok(Self {
            prompt,
            shown,
            path,
            size,
            edit_from,
        })
    }

    /// The host-only label: the output path and the requested size.
    fn subject(&self) -> String {
        match &self.size {
            Some(size) => format!("{} ({size})", self.shown),
            None => self.shown.clone(),
        }
    }
}

fn valid_size(size: &str) -> bool {
    if size == "auto" {
        return true;
    }
    let Some((width, height)) = size.split_once('x') else {
        return false;
    };
    let dimension = |text: &str| {
        (2..=5).contains(&text.len())
            && !text.starts_with('0')
            && text.bytes().all(|byte| byte.is_ascii_digit())
    };
    dimension(width) && dimension(height)
}

struct ImageGenTool {
    port: Arc<dyn ImageGenPort>,
}

impl ToolExecutor for ImageGenTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let call = Call::parse(
                invocation.arguments().value().as_ref().clone(),
                &context,
                self.port.as_ref(),
            )?;
            let subject = call.subject();
            let _ = context.progress().report(&subject, None, None);
            let outcome = generate(&self.port, &call, &context).await?;
            Ok(outcome.with_presentation_subject(subject))
        })
    }
}

async fn generate(
    port: &Arc<dyn ImageGenPort>,
    call: &Call,
    context: &ToolContext,
) -> Result<ToolOutcome, ToolRuntimeError> {
    // Refused before the paid request; the write below refuses a file that
    // appeared since.
    match read_one_slice(context, call.path.clone(), 1).await {
        Err(ExecutionBackendErrorKind::NotFound) => {}
        Ok(_) | Err(_) => {
            return ToolOutcome::failure_text(format!(
                "image_gen refuses to write {}: the path already exists or is not writable; \
                 choose a new path",
                call.shown
            ));
        }
    }
    let mut edit_from = Vec::with_capacity(call.edit_from.len());
    for (shown, path) in &call.edit_from {
        match read_input(context, path).await {
            Ok(source) => edit_from.push(source),
            Err(reason) => {
                return ToolOutcome::failure_text(format!(
                    "image_gen cannot edit {shown}: {reason}"
                ));
            }
        }
    }
    let request = ImageGenRequest {
        prompt: call.prompt.clone(),
        size: call.size.clone(),
        edit_from,
    };
    let (bytes, requests) = match port
        .generate_counted(request, operation_control(context))
        .await
    {
        Ok(outcome) => (outcome.bytes, outcome.physical_requests),
        Err(error) if error.kind() == RetrievalErrorKind::Cancelled => {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                error.message(),
            ));
        }
        Err(error) => return ToolOutcome::failure_text(format!("image_gen failed: {error}")),
    };
    let Ok(header) = plakous_tools::sniff_image(&bytes) else {
        return ToolOutcome::failure_text(
            "image_gen failed: the provider returned bytes that are not a supported image",
        );
    };
    let written = write_new(context, &call.path, bytes.clone()).await;
    if let Err(message) = written {
        return ToolOutcome::failure_text(format!(
            "image_gen generated an image but could not write {}: {message}",
            call.shown
        ));
    }
    let outcome = written_outcome(call, &header, bytes)?;
    Ok(match requests {
        Some(requests) => outcome.with_helper_requests(requests),
        None => outcome,
    })
}

/// The result of a written image: its text, the inline image when it fits,
/// and the image as the execution value.
fn written_outcome(
    call: &Call,
    header: &plakous_tools::ImageHeader,
    bytes: Vec<u8>,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let dimensions = header.dimensions().map_or_else(
        || String::from("unknown"),
        |(width, height)| format!("{width}x{height}"),
    );
    let mut text = format!(
        "image_gen wrote {}: media_type={} dimensions={dimensions} bytes={}",
        call.shown,
        header.media_type(),
        bytes.len()
    );
    let inline = bytes.len() as u64 <= MAX_STANDARD_IMAGE_BYTES;
    if !inline {
        let _ = write!(
            text,
            "\nThe image is larger than {MAX_STANDARD_IMAGE_BYTES} bytes and is not shown; \
             the file is in the workspace."
        );
    }
    let unreturnable = |error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Execution,
            "image_gen result could not be returned",
            &error,
        )
    };
    let mut parts = vec![ToolResultPart::Text(
        TextBlock::new(text, None).map_err(unreturnable)?,
    )];
    if inline {
        parts.push(ToolResultPart::Image(
            ImageBlock::new(
                header.media_type(),
                MediaSource::bytes(bytes.clone()).map_err(unreturnable)?,
            )
            .map_err(unreturnable)?,
        ));
    }
    let image = ImageValue {
        path: call.shown.clone(),
        media_type: header.media_type().to_owned(),
        bytes,
        dimensions: header.dimensions(),
    };
    Ok(ToolOutcome::success(parts)?.with_execution_value(ExecutionValue::Image(image)))
}

/// The first slice of a workspace file of at most `max` bytes, and whether
/// more follows.
async fn read_one_slice(
    context: &ToolContext,
    path: PathBuf,
    max: u64,
) -> Result<(Vec<u8>, bool), ExecutionBackendErrorKind> {
    let request = WorkspaceReadSliceRequest::start(path, max).map_err(|error| error.kind())?;
    let slice = context
        .environment()
        .read_workspace(request, operation_control(context))
        .await
        .map_err(|error| error.kind())?;
    Ok((slice.bytes().as_ref().to_vec(), slice.next().is_some()))
}

async fn read_input(context: &ToolContext, path: &Path) -> Result<ImageGenSource, String> {
    let (bytes, more) = read_one_slice(context, path.to_path_buf(), MAX_IMAGE_EDIT_INPUT_BYTES)
        .await
        .map_err(|kind| match kind {
            ExecutionBackendErrorKind::NotFound => "the file does not exist".to_owned(),
            _ => "the file cannot be read".to_owned(),
        })?;
    if more {
        return Err(format!(
            "the file exceeds {MAX_IMAGE_EDIT_INPUT_BYTES} bytes"
        ));
    }
    match plakous_tools::sniff_image(&bytes) {
        Ok(header) if header.media_type() != "image/gif" => Ok(ImageGenSource {
            media_type: header.media_type(),
            bytes,
        }),
        _ => Err("the file is not a PNG, JPEG or WebP image".to_owned()),
    }
}

async fn write_new(context: &ToolContext, path: &Path, bytes: Vec<u8>) -> Result<(), String> {
    let mutation = WorkspaceMutation::new(
        path.to_path_buf(),
        WorkspaceExpectedFileState::Missing,
        WorkspaceReplacement::content(bytes),
    )
    .map_err(|error| error.message().as_str().to_owned())?;
    let transaction = WorkspaceMutationTransaction::new([mutation])
        .map_err(|error| error.message().as_str().to_owned())?;
    match context
        .environment()
        .mutate_workspace(transaction, operation_control(context))
        .await
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ExecutionBackendErrorKind::Conflict => {
            Err("the path already exists".to_owned())
        }
        Err(error) => Err(error.message().as_str().to_owned()),
    }
}

#[cfg(test)]
mod tests;
