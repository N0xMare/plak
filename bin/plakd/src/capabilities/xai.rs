use super::ProductCapability;

pub(super) static XAI: ProductCapability = ProductCapability {
    route: &plakous_hosted::capabilities::XAI,
    label: "xAI",
    short_label: "xAI",
    recommendation: None,
    doctor: None,
    preset: None,
};
