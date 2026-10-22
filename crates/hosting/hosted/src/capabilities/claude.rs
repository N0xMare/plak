use super::{RouteCapability, RouteKind};
use crate::binding::ResolvedProtocol;
use plakous::subscriptions::NativeProvider;

pub static CLAUDE: RouteCapability = RouteCapability {
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
