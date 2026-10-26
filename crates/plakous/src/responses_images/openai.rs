use super::{
    Arc, ExecutionOperationControl, ImageGenFuture, ImageGenPort, ImageGenRequest,
    ResponsesImageFlavor, error_of, lowered,
};

/// One session's OpenAI API image generation as an [`ImageGenPort`].
pub struct OpenAiImagePort(Arc<plakous_openai::OpenAiImages>);

impl OpenAiImagePort {
    /// Wraps the session's images executor.
    #[must_use]
    pub fn new(images: plakous_openai::OpenAiImages) -> Self {
        Self(Arc::new(images))
    }
}

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
