use super::ProductCapability;

pub(super) static OPENAI: ProductCapability = ProductCapability {
    route: &plakous_hosted::capabilities::OPENAI,
    label: "OpenAI",
    short_label: "OpenAI",
    recommendation: None,
    doctor: None,
    preset: None,
};
