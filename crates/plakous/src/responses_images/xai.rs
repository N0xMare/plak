use super::{
    Arc, ImageGenFuture, ImageGenPort, ImageGenRequest, OperationContext, ResponsesImageFlavor,
    error_of, lowered,
};

/// One session's xAI API image generation as an [`ImageGenPort`].
pub struct XaiImagePort(Arc<plakous_xai::XaiImages>);

impl XaiImagePort {
    /// Wraps the session's images executor.
    #[must_use]
    pub fn new(images: plakous_xai::XaiImages) -> Self {
        Self(Arc::new(images))
    }
}

impl ImageGenPort for XaiImagePort {
    fn max_edit_images(&self) -> usize {
        ResponsesImageFlavor::Xai.max_edit_images()
    }

    fn takes_size(&self) -> bool {
        ResponsesImageFlavor::Xai.takes_size()
    }

    fn generate(&self, request: ImageGenRequest, control: OperationContext) -> ImageGenFuture {
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
