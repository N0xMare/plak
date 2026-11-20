use super::{RouteCapability, RouteKind};

/// Gateway route facts; the binding, not this table, selects its exact wire.
pub static OPENROUTER: RouteCapability = RouteCapability {
    review_deadline,
    hosted_retrieval,
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

#[allow(
    clippy::unnecessary_wraps,
    reason = "route registry uses the same optional signature for every route"
)]
fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    Some(plakous::openrouter::OPENROUTER_HOSTED_RETRIEVAL)
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "route registry uses the same optional signature for every route"
)]
fn review_deadline() -> Option<u64> {
    Some(plakous::openrouter::OPENROUTER_CHAT_REVIEW_DUE_AT)
}
