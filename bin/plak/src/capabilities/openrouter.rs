use super::ProductCapability;

pub(super) static OPENROUTER: ProductCapability = ProductCapability {
    route: &plakous_hosted::capabilities::OPENROUTER,
    label: "OpenRouter",
    short_label: "OpenRouter",
    recommendation: None,
    doctor: None,
    preset: None,
};
