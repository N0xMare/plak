use super::{RouteCapability, RouteKind};
use crate::binding::ResolvedProtocol;
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

/// Codex subscription route facts, independent of direct `OpenAI` API credentials.
pub static CODEX: RouteCapability = RouteCapability {
    review_deadline,
    hosted_retrieval,
    account: "codex",
    aliases: &["codex", "chatgpt"],
    kind: RouteKind::Native(NativeProvider::Codex),
    protocol: Some(ResolvedProtocol::Responses),
    catalog_label: "ChatGPT/Codex subscription",
    native_ptc_refusal: "this Codex model has no native programmatic tool calling (native-ptc) enrolled; \
             Codex Code Mode is enrolled only on CodeModeOnly rows; use js-ptc, py-ptc or direct",
    native_protocol_vocabulary: true,
    usage_label: Some("Codex"),
    subscription_label: Some("Codex Subscription"),
};

fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    NativeProvider::Codex.hosted_retrieval()
}

fn review_deadline() -> Option<u64> {
    NativeSubscriptions::notice(NativeProvider::Codex)
        .ok()
        .map(plakous::subscriptions::CompatibilityNotice::expires_at)
}
