use super::{RouteCapability, RouteKind};
use crate::binding::ResolvedProtocol;
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

/// Claude subscription route facts; never interchangeable with an API-key route.
pub static CLAUDE: RouteCapability = RouteCapability {
    review_deadline,
    hosted_retrieval,
    account: "claude",
    aliases: &["claude"],
    kind: RouteKind::Native(NativeProvider::Claude),
    protocol: Some(ResolvedProtocol::Messages),
    catalog_label: "Claude subscription",
    native_ptc_refusal: "Claude has no native programmatic tool calling (native-ptc) enrolled; \
             use js-ptc, py-ptc or direct",
    native_protocol_vocabulary: true,
    usage_label: Some("Claude"),
    subscription_label: Some("Claude Subscription"),
};

fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    NativeProvider::Claude.hosted_retrieval()
}

fn review_deadline() -> Option<u64> {
    NativeSubscriptions::notice(NativeProvider::Claude)
        .ok()
        .map(plakous::subscriptions::CompatibilityNotice::expires_at)
}
