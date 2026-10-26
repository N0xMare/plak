#[cfg(feature = "anthropic")]
#[path = "anthropic_available.rs"]
mod compiled;
#[cfg(not(feature = "anthropic"))]
#[path = "anthropic_unavailable.rs"]
mod compiled;

use super::{RouteCapability, RouteKind};
use crate::binding::PlatformProvider;
use crate::binding::ResolvedProtocol;

pub static ANTHROPIC: RouteCapability = RouteCapability {
    compiled: compiled::COMPILED,
    feature: "anthropic",
    review_deadline: compiled::review_deadline,
    hosted_retrieval: compiled::hosted_retrieval,
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
