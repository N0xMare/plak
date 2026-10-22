use super::ProductCapability;

pub(super) static ANTHROPIC: ProductCapability = ProductCapability {
    route: &plakous_hosted::capabilities::ANTHROPIC,
    label: "Anthropic",
    short_label: "Anthropic",
    recommendation: None,
    doctor: None,
    preset: None,
};
