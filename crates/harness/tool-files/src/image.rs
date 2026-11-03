use crate::text::{PLAKOUS_TOOL_IMAGE_TOO_LARGE, PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION};
use plakous_model::{
    ImageBlock, MediaSource, TextBlock, ToolConcurrency, ToolDefinition, ToolEffect, ToolResultPart,
};
use plakous_tools::object_schema;
use plakous_tools::standard_definition;
use plakous_tools::{INSPECT_IMAGE_CAPABILITY_NAME, MAX_STANDARD_IMAGE_BYTES};
use plakous_tools::{
    ToolContext, ToolExecutor, ToolFuture, ToolInvocation, ToolOutcome, ToolRuntimeError,
    ToolRuntimeErrorKind, WorkspaceReadSliceRequest,
};
use plakous_tools::{
    backend_runtime_error, content_runtime_error, decode_arguments, operation_context,
};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

use crate::relative_workspace_path;

#[derive(Clone)]
pub(super) struct InspectImageTool;

impl InspectImageTool {
    pub(super) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        Ok((
            standard_definition(
                INSPECT_IMAGE_CAPABILITY_NAME,
                PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION,
                object_schema(
                    json!({
                        "path": {"type": "string"}
                    }),
                    &["path"],
                ),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            Self,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectImageArgs {
    path: PathBuf,
}

impl ToolExecutor for InspectImageTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: InspectImageArgs = decode_arguments(&invocation)?;
            context
                .progress()
                .report("reading workspace image", None, None)?;
            let path = relative_workspace_path(&context, args.path)?;
            let result = context
                .environment()
                .read_workspace(
                    WorkspaceReadSliceRequest::start(path, MAX_STANDARD_IMAGE_BYTES)
                        .map_err(backend_runtime_error)?,
                    operation_context(&context),
                )
                .await
                .map_err(backend_runtime_error)?;
            if result.next().is_some() {
                return ToolOutcome::failure_text(PLAKOUS_TOOL_IMAGE_TOO_LARGE);
            }
            let info = inspect_image_header(result.bytes())?;
            let dimensions = info.dimensions().map_or_else(
                || String::from("unknown"),
                |(width, height)| format!("{width}x{height}"),
            );
            let metadata = TextBlock::new(
                format!(
                    "media_type={} bytes={} dimensions={dimensions}",
                    info.media_type(),
                    result.bytes().len()
                ),
                None,
            )
            .map_err(content_runtime_error)?;
            let image = ImageBlock::new(
                info.media_type(),
                MediaSource::bytes(result.bytes().as_ref().to_vec())
                    .map_err(content_runtime_error)?,
            )
            .map_err(content_runtime_error)?;
            ToolOutcome::success(vec![
                ToolResultPart::Text(metadata),
                ToolResultPart::Image(image),
            ])
        })
    }
}

fn inspect_image_header(bytes: &[u8]) -> Result<plakous_tools::ImageHeader, ToolRuntimeError> {
    plakous_tools::sniff_image(bytes).map_err(|error| match error {
        plakous_tools::ImageSniffError::Dimensions => ToolRuntimeError::new(
            ToolRuntimeErrorKind::ResourceLimit,
            "image dimensions exceed the configured limit",
        ),
        _ => ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "workspace file is not a supported image",
        ),
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::inspect_image_header;
    use plakous_tools::ToolRuntimeError;

    #[test]
    fn image_headers_are_signature_bound_and_dimension_limited() -> Result<(), ToolRuntimeError> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0; 8]);
        png.extend_from_slice(&2_u32.to_be_bytes());
        png.extend_from_slice(&3_u32.to_be_bytes());
        let info = inspect_image_header(&png)?;
        assert_eq!(info.media_type(), "image/png");
        assert_eq!(info.dimensions(), Some((2, 3)));
        assert!(inspect_image_header(b"not-an-image").is_err());
        Ok(())
    }
}
