use super::{RouteCapability, RouteKind};
use crate::binding::ResolvedProtocol;
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

/// Grok subscription route facts, independent of direct xAI API credentials.
pub static GROK: RouteCapability = RouteCapability {
    review_deadline: review_deadline,
    hosted_retrieval: hosted_retrieval,
    account: "grok",
    aliases: &["grok", "supergrok", "xai-oauth"],
    kind: RouteKind::Native(NativeProvider::Xai),
    protocol: Some(ResolvedProtocol::Responses),
    catalog_label: "SuperGrok subscription",
    native_ptc_refusal: "Grok has no native programmatic tool calling (native-ptc) enrolled; \
             use js-ptc, py-ptc or direct",
    native_protocol_vocabulary: true,
    usage_label: Some("SuperGrok"),
    subscription_label: Some("Grok Subscription"),
};

pub(crate) const fn tool_result_images() -> bool {
    plakous::xai::SUPERGROK_ALLOW_TOOL_RESULT_IMAGES
}
fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    NativeProvider::Xai.hosted_retrieval()
}

fn review_deadline() -> Option<u64> {
    NativeSubscriptions::notice(NativeProvider::Xai)
        .ok()
        .map(plakous::subscriptions::CompatibilityNotice::expires_at)
}
