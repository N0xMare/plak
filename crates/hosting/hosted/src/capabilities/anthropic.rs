use super::{RouteCapability, RouteKind};
use crate::binding::PlatformProvider;
use crate::binding::ResolvedProtocol;

/// Anthropic direct-API route facts, readable even when its runtime is omitted.
pub static ANTHROPIC: RouteCapability = RouteCapability {
    review_deadline,
    hosted_retrieval,
    account: "anthropic",
    aliases: &["anthropic"],
    kind: RouteKind::Platform(PlatformProvider::Anthropic),
    protocol: Some(ResolvedProtocol::Messages),
    catalog_label: "Anthropic API",
    native_ptc_refusal: "the Anthropic API has no native programmatic tool calling (native-ptc) enrolled; \
                 use js-ptc, py-ptc or direct",
    native_protocol_vocabulary: false,
    usage_label: None,
    subscription_label: None,
};

#[allow(
    clippy::unnecessary_wraps,
    reason = "route registry uses the same optional signature for every route"
)]
fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    Some(plakous::anthropic::ANTHROPIC_API_HOSTED_RETRIEVAL)
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "route registry uses the same optional signature for every route"
)]
fn review_deadline() -> Option<u64> {
    Some(plakous::anthropic::ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_UNIX_SECONDS)
}
