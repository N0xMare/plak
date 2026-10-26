#[cfg(feature = "openai")]
#[path = "openai_available.rs"]
mod compiled;
#[cfg(not(feature = "openai"))]
#[path = "openai_unavailable.rs"]
mod compiled;

use super::{RouteCapability, RouteKind};
use crate::binding::PlatformProvider;
use crate::binding::ResolvedProtocol;

pub static OPENAI: RouteCapability = RouteCapability {
    compiled: compiled::COMPILED,
    feature: "openai",
    review_deadline: compiled::review_deadline,
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
