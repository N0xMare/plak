use super::{RouteCapability, RouteKind};
use crate::binding::PlatformProvider;
use crate::binding::ResolvedProtocol;

/// xAI direct-API route facts, independent of Grok subscription credentials.
pub static XAI: RouteCapability = RouteCapability {
    review_deadline: review_deadline,
    hosted_retrieval: hosted_retrieval,
    account: "xai-api",
    aliases: &["xai-api"],
    kind: RouteKind::Platform(PlatformProvider::Xai),
    protocol: Some(ResolvedProtocol::Responses),
    catalog_label: "xAI API",
    native_ptc_refusal: "the xAI API has no native programmatic tool calling (native-ptc) enrolled; \
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
    Some(plakous::xai::XAI_API_HOSTED_RETRIEVAL)
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "route registry uses the same optional signature for every route"
)]
fn review_deadline() -> Option<u64> {
    Some(plakous::xai::XAI_PLATFORM_REVIEW_DUE_AT_UNIX_SECONDS)
}
