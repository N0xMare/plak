//! The neutral image generation port over the OpenAI API, Codex and xAI
//! API images executors.

use std::sync::Arc;

use plakous_tool_web::{ImageGenFuture, ImageGenPort, ImageGenRequest};
use plakous_tools::OperationContext;

#[cfg(any(feature = "openai", feature = "codex"))]
use plakous_openai as responses;
#[cfg(all(
    any(feature = "xai", feature = "grok"),
    not(any(feature = "openai", feature = "codex"))
))]
use plakous_xai as responses;
use responses::{ResponsesImageFlavor, ResponsesImageRequest, ResponsesImageSource};

use crate::retrieval_outcome::error_of;

#[cfg(feature = "openai")]
mod openai;
#[cfg(feature = "openai")]
pub use openai::OpenAiImagePort;

#[cfg(feature = "codex")]
mod codex;
#[cfg(feature = "codex")]
pub use codex::CodexImagePort;

#[cfg(any(feature = "xai", feature = "grok"))]
mod xai;
#[cfg(any(feature = "xai", feature = "grok"))]
pub use xai::XaiImagePort;

fn lowered(
    model: &str,
    flavor: ResponsesImageFlavor,
    request: ImageGenRequest,
) -> ResponsesImageRequest {
    ResponsesImageRequest::new(model, flavor, request.prompt)
        .with_size(request.size)
        .with_edit_from(
            request
                .edit_from
                .into_iter()
                .map(|source| ResponsesImageSource {
                    media_type: source.media_type.to_owned(),
                    bytes: source.bytes,
                })
                .collect(),
        )
}

#[cfg(test)]
mod tests {
    use plakous_tool_web::ImageGenSource;

    use super::*;

    #[test]
    fn requests_lower_onto_the_route_body() -> Result<(), Box<dyn std::error::Error>> {
        let request = ImageGenRequest {
            prompt: "a red fox".to_owned(),
            size: Some("1024x1024".to_owned()),
            edit_from: vec![ImageGenSource {
                media_type: "image/png",
                bytes: b"ab".to_vec(),
            }],
        };
        let body = lowered("gpt-image-2", ResponsesImageFlavor::OpenAi, request).body()?;
        assert_eq!(
            String::from_utf8(body)?,
            concat!(
                r#"{"images":[{"image_url":"data:image/png;base64,YWI="}],"#,
                r#""model":"gpt-image-2","n":1,"prompt":"a red fox","size":"1024x1024"}"#
            )
        );
        Ok(())
    }
}
