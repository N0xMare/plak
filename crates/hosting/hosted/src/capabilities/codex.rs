use super::{RouteCapability, RouteKind};
use crate::binding::ResolvedProtocol;
use plakous::subscriptions::NativeProvider;

pub static CODEX: RouteCapability = RouteCapability {
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
