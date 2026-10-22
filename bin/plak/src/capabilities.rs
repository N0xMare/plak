//! Product presentation attached to the hosted route registry.

use plakous_host_protocol::views::AuthKind;
use plakous_hosted::capabilities::{RouteCapability, RouteKind};

use crate::product_doctor::{CheckKind, OauthCredentialProbe, OauthCredentialProbes};
use crate::profiles::Builtin;

mod anthropic;
mod claude;
mod codex;
mod grok;
mod openai;
mod openrouter;
mod xai;

pub(crate) const RECOMMENDED_CODEX_MODEL: &str = codex::RECOMMENDED_MODEL;

pub(crate) struct Recommendation {
    pub accounts: &'static [&'static str],
    pub model: &'static str,
}

pub(crate) enum RouteDoctor {
    Oauth {
        kind: CheckKind,
        probe: fn(&OauthCredentialProbes) -> OauthCredentialProbe,
    },
    OpenRouterCatalog,
}

pub(crate) struct ProductCapability {
    pub route: &'static RouteCapability,
    pub label: &'static str,
    pub short_label: &'static str,
    pub recommendation: Option<Recommendation>,
    pub doctor: Option<RouteDoctor>,
    /// Preserve the profile menu's order independently of catalog order.
    pub preset: Option<(u8, &'static Builtin)>,
}

impl ProductCapability {
    pub(crate) fn auth_kind(&self) -> AuthKind {
        match self.route.kind {
            RouteKind::Native(_) => AuthKind::Oauth,
            RouteKind::Platform(_) | RouteKind::OpenRouter => AuthKind::ApiKey,
        }
    }
}

pub(crate) const ROUTES: &[&ProductCapability] = &[
    &codex::CODEX,
    &grok::GROK,
    &claude::CLAUDE,
    &openai::OPENAI,
    &xai::XAI,
    &anthropic::ANTHROPIC,
    &openrouter::OPENROUTER,
];

pub(crate) fn oauth_accounts() -> impl Iterator<Item = &'static str> {
    ROUTES
        .iter()
        .filter(|capability| capability.auth_kind() == AuthKind::Oauth)
        .map(|capability| capability.route.account)
}

pub(crate) fn usage_verticals() -> impl Iterator<Item = (&'static str, &'static str)> {
    ROUTES.iter().filter_map(|capability| {
        capability
            .route
            .usage_label
            .map(|label| (capability.route.account, label))
    })
}

pub(crate) fn recommended_model(account: &str) -> Option<&'static str> {
    ROUTES.iter().find_map(|capability| {
        let recommendation = capability.recommendation.as_ref()?;
        recommendation
            .accounts
            .contains(&account)
            .then_some(recommendation.model)
    })
}

pub(crate) fn presets() -> impl Iterator<Item = &'static Builtin> {
    let mut presets: Vec<_> = ROUTES
        .iter()
        .filter_map(|capability| capability.preset)
        .collect();
    presets.sort_by_key(|(order, _)| *order);
    presets.into_iter().map(|(_, preset)| preset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_registry_is_the_hosted_registry_in_the_same_order() {
        assert_eq!(ROUTES.len(), plakous_hosted::capabilities::ROUTES.len());
        for (product, hosted) in ROUTES.iter().zip(plakous_hosted::capabilities::ROUTES) {
            assert!(std::ptr::eq(product.route, *hosted));
        }
        assert_eq!(
            oauth_accounts().collect::<Vec<_>>(),
            ["codex", "grok", "claude"]
        );
        assert_eq!(
            usage_verticals().collect::<Vec<_>>(),
            [
                ("codex", "Codex"),
                ("grok", "SuperGrok"),
                ("claude", "Claude"),
                ("openrouter", "OpenRouter"),
            ]
        );
        assert_eq!(
            presets().map(|preset| preset.id).collect::<Vec<_>>(),
            ["codex-default", "claude-default", "grok-default"]
        );
    }

    #[test]
    fn recommendation_aliases_do_not_expand_account_admission() {
        assert_eq!(recommended_model("xai"), Some("grok-4.6"));
        assert_eq!(recommended_model("xai-oauth"), None);
        assert_eq!(recommended_model("chatgpt"), Some(RECOMMENDED_CODEX_MODEL));
        assert_eq!(recommended_model("openai"), None);
        assert!(plakous_hosted::capabilities::for_account("xai").is_none());
    }

    #[test]
    fn route_doctors_include_native_credentials_and_gateway_catalog() {
        let doctors: Vec<_> = ROUTES
            .iter()
            .filter_map(|capability| {
                capability.doctor.as_ref().map(|doctor| match doctor {
                    RouteDoctor::Oauth { kind, .. } => kind.key(),
                    RouteDoctor::OpenRouterCatalog => CheckKind::OpenRouterCatalog.key(),
                })
            })
            .collect();
        assert_eq!(
            doctors,
            [
                "oauth_codex",
                "oauth_xai",
                "oauth_claude",
                "openrouter_catalog"
            ]
        );
    }

    #[test]
    fn complete_binding_vocabularies_keep_the_frozen_membership_sets() {
        use std::collections::BTreeSet;
        assert_eq!(
            crate::product_config::template_accounts().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "codex",
                "chatgpt",
                "grok",
                "supergrok",
                "xai-oauth",
                "claude",
                "openrouter",
                "openai",
                "anthropic",
                "xai-api"
            ])
        );
        assert_eq!(
            crate::product_config::no_native_protocol_accounts().collect::<BTreeSet<_>>(),
            BTreeSet::from(["openrouter", "anthropic", "xai-api"])
        );
    }
}
