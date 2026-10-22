use super::{RouteCapability, RouteKind};
use crate::binding::ResolvedProtocol;
use plakous::subscriptions::NativeProvider;

pub static GROK: RouteCapability = RouteCapability {
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
