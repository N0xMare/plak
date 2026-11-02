use std::sync::Arc;

use crate::ImageNormalizer;

/// Live route facts for image tool reads.
///
/// This is not part of the execution-environment descriptor. The agent derives
/// the route, model and image-input fact from the bound provider configuration
/// and installs them with the host's normalizer through
/// [`crate::internal::install_visual_read`]. Reconstructed environments start
/// without it until that installation.
#[derive(Clone)]
pub struct VisualReadSession {
    route: String,
    model: String,
    accepts_images: bool,
    normalizer: Option<Arc<dyn ImageNormalizer>>,
}

impl VisualReadSession {
    pub(crate) fn new(
        route: impl Into<String>,
        model: impl Into<String>,
        accepts_images: bool,
        normalizer: Option<Arc<dyn ImageNormalizer>>,
    ) -> Self {
        Self {
            route: route.into(),
            model: model.into(),
            accepts_images,
            normalizer,
        }
    }

    /// Whether the bound model accepts image input.
    #[must_use]
    pub const fn accepts_images(&self) -> bool {
        self.accepts_images
    }

    /// The installed image normalizer, when this build can decode images.
    #[must_use]
    pub fn normalizer(&self) -> Option<Arc<dyn ImageNormalizer>> {
        self.normalizer.clone()
    }

    /// Codex `view_image` on a model that does not accept images.
    #[must_use]
    pub fn view_image_refusal(&self) -> String {
        format!(
            "view_image is not allowed because you do not support image inputs; route {} model {}",
            self.route, self.model
        )
    }

    /// Claude `Read` on a model that does not accept images.
    #[must_use]
    pub fn claude_image_refusal(&self) -> String {
        format!(
            "route {} model {} does not accept image input; Read cannot return images on this model",
            self.route, self.model
        )
    }
}
