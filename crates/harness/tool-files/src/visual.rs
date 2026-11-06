//! Shared image reads for Claude `Read` and Codex `view_image`.
//!
//! Both use the workspace path rule. Job-output paths are not visual reads:
//! Claude `Read` leaves them on the text path, and `view_image` has no
//! job-output exception. A successful image is not a text-read observation.

use std::path::PathBuf;
use std::sync::Arc;

use plakous_model::{
    ImageBlock, ImageDetailHint, MediaSource, ToolConcurrency, ToolDefinition, ToolEffect,
};
use plakous_tools::internal::VisualReadSession;
use plakous_tools::{
    ImageSniffError, MAX_WORKSPACE_READ_SLICE_BYTES, ToolContext, ToolOutcome, ToolRuntimeError,
    ToolRuntimeErrorKind, WorkspaceReadSliceRequest, backend_runtime_error, content_runtime_error,
    operation_context, sniff_image,
};
use plakous_tools::{VIEW_IMAGE_CAPABILITY_NAME, object_schema, standard_definition};

use crate::VIEW_IMAGE_DESCRIPTION;
use crate::relative_workspace_path;

use plakous_tools::{ToolExecutor, ToolFuture, ToolInvocation, decode_arguments};
use serde::Deserialize;
use serde_json::json;

const PATH_DESCRIPTION: &str = "Local filesystem path to an image file.";

pub(super) fn view_image_schema() -> serde_json::Value {
    object_schema(
        json!({
            "path": {"type": "string", "description": PATH_DESCRIPTION}
        }),
        &["path"],
    )
}

#[derive(Clone)]
pub(super) struct ViewImageTool;

impl ViewImageTool {
    pub(super) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        Ok((
            standard_definition(
                VIEW_IMAGE_CAPABILITY_NAME,
                VIEW_IMAGE_DESCRIPTION,
                view_image_schema(),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            Self,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewImageArgs {
    path: PathBuf,
}

impl ToolExecutor for ViewImageTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: ViewImageArgs = decode_arguments(&invocation)?;
            let subject: String = args.path.display().to_string().chars().take(100).collect();
            context.progress().report(&subject, None, None)?;
            view_image(&context, &args.path).await
        })
    }
}

const UNSUPPORTED_IMAGE: &str = "unable to process image: invalid or unsupported image data";
const IMAGE_READS_UNAVAILABLE: &str = "image reads are unavailable in this build";
const IMAGE_BUDGET: &str = "image does not fit the 1 MiB tool-result budget even downscaled";

/// First read used to decide whether the file is an image. A non-image stops
/// here so the text read is the only read of the rest of the file. An image
/// is read whole: the workspace read refuses a file over its 16 MiB bound
/// before the first slice, and every resume is pinned to that content.
const VISUAL_SNIFF_BYTES: u64 = 8 * 1024;

fn is_image(bytes: &[u8]) -> bool {
    match sniff_image(bytes) {
        Ok(_) | Err(ImageSniffError::Dimensions) => true,
        Err(_) => false,
    }
}

enum Loaded {
    NotImage,
    Image(Vec<u8>),
}

async fn load_image(context: &ToolContext, path: PathBuf) -> Result<Loaded, ToolRuntimeError> {
    let request = WorkspaceReadSliceRequest::start(path, VISUAL_SNIFF_BYTES)
        .map_err(backend_runtime_error)?;
    context.check_cancelled()?;
    let slice = context
        .environment()
        .read_workspace(request, operation_context(context))
        .await
        .map_err(backend_runtime_error)?;
    if !is_image(slice.bytes()) {
        return Ok(Loaded::NotImage);
    }
    let mut bytes = slice.bytes().to_vec();
    let mut next = slice.next().cloned();
    while let Some(cursor) = next {
        let request = WorkspaceReadSliceRequest::resume(cursor, MAX_WORKSPACE_READ_SLICE_BYTES)
            .map_err(backend_runtime_error)?;
        context.check_cancelled()?;
        let slice = context
            .environment()
            .read_workspace(request, operation_context(context))
            .await
            .map_err(backend_runtime_error)?;
        bytes.extend_from_slice(slice.bytes());
        next = slice.next().cloned();
    }
    Ok(Loaded::Image(bytes))
}

async fn image_outcome(
    context: &ToolContext,
    bytes: Vec<u8>,
    refusal: impl Fn(&VisualReadSession) -> String,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let Some(session) = plakous_tools::internal::visual_read(context.environment()) else {
        return ToolOutcome::failure_text(IMAGE_READS_UNAVAILABLE);
    };
    if !session.accepts_images() {
        return ToolOutcome::failure_text(refusal(session));
    }
    let Some(normalizer) = session.normalizer() else {
        return ToolOutcome::failure_text(IMAGE_READS_UNAVAILABLE);
    };
    context.check_cancelled()?;
    let prepared = plakous_tools::internal::normalize_tool_image(
        normalizer,
        Ok(bytes),
        context.cancellation(),
    )
    .await
    .map_err(|error| {
        if error.kind() == ToolRuntimeErrorKind::Cancelled {
            error
        } else {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "image normalization task failed",
            )
        }
    })?;
    context.check_cancelled()?;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return ToolOutcome::failure_text(error),
    };
    let image = ImageBlock::with_detail(
        prepared.media_type(),
        MediaSource::bytes(prepared.bytes().to_vec()).map_err(content_runtime_error)?,
        Some(ImageDetailHint::High),
    )
    .map_err(content_runtime_error)?;
    match ToolOutcome::success(vec![plakous_model::ToolResultPart::Image(image)]) {
        Ok(outcome) => Ok(plakous_tools::internal::mark_tool_images_prepared(outcome)),
        Err(_) => ToolOutcome::failure_text(IMAGE_BUDGET),
    }
}

/// Claude `Read`: an image becomes model content. Anything else is `None` and
/// stays on the text read. A missing workspace file is an error.
pub(super) async fn claude_read(
    context: &ToolContext,
    path: &std::path::Path,
) -> Result<Option<ToolOutcome>, ToolRuntimeError> {
    let Ok(path) = relative_workspace_path(context, path) else {
        return Ok(None);
    };
    let outcome = match load_image(context, path).await? {
        Loaded::NotImage => return Ok(None),
        Loaded::Image(bytes) => {
            image_outcome(context, bytes, VisualReadSession::claude_image_refusal).await?
        }
    };
    Ok(Some(outcome))
}

/// Codex `view_image`: one workspace image, or a refusal.
pub(super) async fn view_image(
    context: &ToolContext,
    path: &std::path::Path,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let path = relative_workspace_path(context, path)?;
    match load_image(context, path).await? {
        Loaded::NotImage => ToolOutcome::failure_text(UNSUPPORTED_IMAGE),
        Loaded::Image(bytes) => {
            image_outcome(context, bytes, VisualReadSession::view_image_refusal).await
        }
    }
}

#[cfg(test)]
mod tests {
    const _: () = assert!(super::VISUAL_SNIFF_BYTES <= 8 * 1024);

    #[test]
    fn a_short_prefix_distinguishes_images_from_text() {
        assert!(!super::is_image(b"hello"));
        assert!(!super::is_image(b"%PDF-1.7\n"));
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1_u32.to_be_bytes());
        png.extend_from_slice(&1_u32.to_be_bytes());
        assert!(super::is_image(&png));
    }
}
