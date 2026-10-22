use super::{RouteCapability, RouteKind};

pub static OPENROUTER: RouteCapability = RouteCapability {
    account: "openrouter",
    aliases: &["openrouter"],
    kind: RouteKind::OpenRouter,
    protocol: None,
    catalog_label: "OpenRouter",
    native_ptc_refusal: "OpenRouter has no native programmatic tool calling (native-ptc) enrolled; \
     use js-ptc, py-ptc or direct",
    native_protocol_vocabulary: false,
    usage_label: Some("OpenRouter"),
    subscription_label: None,
};
