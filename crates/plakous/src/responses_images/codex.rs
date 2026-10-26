use super::{
    Arc, ExecutionOperationControl, ImageGenFuture, ImageGenPort, ImageGenRequest,
    ResponsesImageFlavor, error_of, lowered,
};

/// One session's Codex subscription image generation as an [`ImageGenPort`].
pub struct CodexImagePort(Arc<plakous_openai::CodexImages>);

impl CodexImagePort {
    /// Wraps the session's images executor.
    #[must_use]
    pub fn new(images: plakous_openai::CodexImages) -> Self {
        Self(Arc::new(images))
    }
}

impl ImageGenPort for CodexImagePort {
    fn max_edit_images(&self) -> usize {
        ResponsesImageFlavor::Codex.max_edit_images()
    }

    fn takes_size(&self) -> bool {
        ResponsesImageFlavor::Codex.takes_size()
    }

    fn generate(
        &self,
        request: ImageGenRequest,
        control: ExecutionOperationControl,
    ) -> ImageGenFuture {
        let images = Arc::clone(&self.0);
        Box::pin(async move {
            let request = lowered(
                plakous_openai::CODEX_IMAGE_MODEL,
                ResponsesImageFlavor::Codex,
                request,
            );
            images
                .generate(&request, control.cancellation())
                .await
                .map_err(|error| error_of(&error))
        })
    }
}
