use super::{ProductCapability, RouteDoctor};

pub(super) static OPENROUTER: ProductCapability = ProductCapability {
    route: &plakous_hosted::capabilities::OPENROUTER,
    label: "OpenRouter",
    short_label: "OpenRouter",
    recommendation: None,
    doctor: Some(RouteDoctor::OpenRouterCatalog),
    preset: None,
};
