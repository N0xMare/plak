use super::{RouteCapability, RouteKind};
use crate::binding::PlatformProvider;
use crate::binding::ResolvedProtocol;

pub static XAI: RouteCapability = RouteCapability {
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
