use super::{RouteCapability, RouteKind};
use crate::binding::PlatformProvider;
use crate::binding::ResolvedProtocol;

/// `OpenAI` direct-API route facts, independent of a `ChatGPT` subscription.
pub static OPENAI: RouteCapability = RouteCapability {
    review_deadline: review_deadline,
    hosted_retrieval: hosted_retrieval,
    account: "openai",
    aliases: &["openai"],
    kind: RouteKind::Platform(PlatformProvider::OpenAi),
    protocol: Some(ResolvedProtocol::Responses),
    catalog_label: "OpenAI API",
    native_ptc_refusal: "this OpenAI API model has no native programmatic tool calling (native-ptc) enrolled; \
                 Codex Code Mode is enrolled only on its CodeModeOnly models; use js-ptc, py-ptc or direct",
    native_protocol_vocabulary: true,
    usage_label: None,
    subscription_label: None,
};

#[allow(
    clippy::unnecessary_wraps,
    reason = "route registry uses the same optional signature for every route"
)]
fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    Some(plakous::openai::OPENAI_API_HOSTED_RETRIEVAL)
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "route registry uses the same optional signature for every route"
)]
fn review_deadline() -> Option<u64> {
    Some(plakous::openai::OPENAI_PLATFORM_MODEL_MANIFEST_REVIEW_DUE_AT_UNIX_SECONDS)
}
