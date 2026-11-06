#[cfg(feature = "grok")]
#[path = "grok_available.rs"]
mod compiled;
#[cfg(not(feature = "grok"))]
#[path = "grok_unavailable.rs"]
mod compiled;

use super::{RouteCapability, RouteKind};
use crate::binding::ResolvedProtocol;
use plakous::subscriptions::NativeProvider;

pub(crate) use compiled::tool_result_images;

pub static GROK: RouteCapability = RouteCapability {
    compiled: compiled::COMPILED,
    feature: "grok",
    review_deadline: compiled::review_deadline,
    hosted_retrieval: compiled::hosted_retrieval,
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
