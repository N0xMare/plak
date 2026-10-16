//! The neutral image generation port over the OpenAI API and xAI API
//! images executors.

use std::sync::Arc;

use plakous_tool_web::{ImageGenFuture, ImageGenPort, ImageGenRequest};
use plakous_tools::ExecutionOperationControl;

#[cfg(feature = "openai")]
use plakous_openai as responses;
#[cfg(all(feature = "xai", not(feature = "openai")))]
use plakous_xai as responses;
use responses::{ResponsesImageFlavor, ResponsesImageRequest, ResponsesImageSource};

use crate::responses_retrieval::error_of;

/// One session's OpenAI API image generation as an [`ImageGenPort`].
#[cfg(feature = "openai")]
pub struct OpenAiImagePort(Arc<plakous_openai::OpenAiImages>);

#[cfg(feature = "openai")]
impl OpenAiImagePort {
    /// Wraps the session's images executor.
    #[must_use]
    pub fn new(images: plakous_openai::OpenAiImages) -> Self {
        Self(Arc::new(images))
    }
}

#[cfg(feature = "openai")]
impl ImageGenPort for OpenAiImagePort {
    fn max_edit_images(&self) -> usize {
        ResponsesImageFlavor::OpenAi.max_edit_images()
    }

    fn takes_size(&self) -> bool {
        ResponsesImageFlavor::OpenAi.takes_size()
    }

    fn generate(
        &self,
        request: ImageGenRequest,
        control: ExecutionOperationControl,
    ) -> ImageGenFuture {
        let images = Arc::clone(&self.0);
        Box::pin(async move {
            let request = lowered(
                plakous_openai::OPENAI_IMAGE_MODEL,
                ResponsesImageFlavor::OpenAi,
                request,
            );
            images
                .generate(&request, control.cancellation())
                .await
                .map_err(|error| error_of(&error))
        })
    }
}

/// One session's xAI API image generation as an [`ImageGenPort`].
#[cfg(feature = "xai")]
pub struct XaiImagePort(Arc<plakous_xai::XaiImages>);

#[cfg(feature = "xai")]
impl XaiImagePort {
    /// Wraps the session's images executor.
    #[must_use]
    pub fn new(images: plakous_xai::XaiImages) -> Self {
        Self(Arc::new(images))
    }
}

#[cfg(feature = "xai")]
impl ImageGenPort for XaiImagePort {
    fn max_edit_images(&self) -> usize {
        ResponsesImageFlavor::Xai.max_edit_images()
    }

    fn takes_size(&self) -> bool {
        ResponsesImageFlavor::Xai.takes_size()
    }

    fn generate(
        &self,
        request: ImageGenRequest,
        control: ExecutionOperationControl,
    ) -> ImageGenFuture {
        let images = Arc::clone(&self.0);
        Box::pin(async move {
            let request = lowered(
                plakous_xai::XAI_IMAGE_MODEL,
                ResponsesImageFlavor::Xai,
                request,
            );
            images
                .generate(&request, control.cancellation())
                .await
                .map_err(|error| error_of(&error))
        })
    }
}

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
