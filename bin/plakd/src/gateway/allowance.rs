//! OpenRouter allowance is one `GET /api/v1/key` from this process. Other
//! API-key kinds have no provider allowance endpoint and stay unobserved.
//! The key never enters the admin JSON.

use plakous::gateway::protocol::admin::AdminResponse;
use plakous::gateway::protocol::{BoundedText, GatewayAccount};

use crate::home::paths::PlakPaths;

pub(super) async fn read(home: &PlakPaths, account: &GatewayAccount) -> AdminResponse {
    if account.kind() == "openrouter" {
        return openrouter::read(home, account).await;
    }
    not_observed(account)
}

fn not_observed(account: &GatewayAccount) -> AdminResponse {
    AdminResponse::Allowance {
        account: account.clone(),
        observed_at_unix_ms: None,
        view: None,
        unavailable: bounded("not-observed"),
    }
}

fn bounded(text: &str) -> Option<BoundedText<64>> {
    BoundedText::new(text.to_owned()).ok()
}

#[cfg(feature = "openrouter")]
mod openrouter {
    use std::sync::Arc;

    use super::{GatewayAccount, PlakPaths, bounded, not_observed};
    use plakous::gateway::protocol::admin::AdminResponse;
    use plakous::openrouter::{OpenRouterChatClient, OpenRouterRetentionPolicy};

    pub(super) async fn read(home: &PlakPaths, account: &GatewayAccount) -> AdminResponse {
        let Some(parsed) = account
            .as_str()
            .parse::<plakous_hosted::account::AccountRef>()
            .ok()
            .filter(|parsed| parsed.vertical() == plakous_hosted::account::Vertical::OpenRouter)
        else {
            return not_observed(account);
        };
        let Some(value) = crate::home::secrets::resolve_api_key(home, account.as_str())
            .ok()
            .flatten()
        else {
            return not_observed(account);
        };
        let Ok(key) = plakous::openrouter::ApiKey::new(value) else {
            return not_observed(account);
        };
        let Ok(source) = plakous::openrouter::static_openrouter_api_key_source(
            key,
            "plak.openrouter",
            parsed.source_authority("plak.product-secrets.openrouter.v1"),
        ) else {
            return not_observed(account);
        };
        let Ok(client) = OpenRouterChatClient::connect_with_retention(
            Arc::new(source),
            OpenRouterRetentionPolicy::gateway_default(),
        ) else {
            return not_observed(account);
        };
        match client.key_usage().await {
            Ok(usage) => observed(account, &usage),
            Err(_) => unavailable(account, "provider-rejected"),
        }
    }

    fn observed(
        account: &GatewayAccount,
        usage: &plakous::openrouter::OpenRouterKeyUsage,
    ) -> AdminResponse {
        let observed_at = usage.observed_at();
        let observed_at_unix_ms = observed_at
            .seconds()
            .saturating_mul(1000)
            .saturating_add(u64::from(observed_at.nanoseconds()) / 1_000_000);
        AdminResponse::Allowance {
            account: account.clone(),
            observed_at_unix_ms: Some(observed_at_unix_ms),
            view: Some(serde_json::json!({
                "limit": usage.limit(),
                "limit_remaining": usage.limit_remaining(),
                "limit_reset": usage.limit_reset(),
                "usage": usage.usage(),
                "usage_daily": usage.usage_daily(),
                "usage_weekly": usage.usage_weekly(),
                "usage_monthly": usage.usage_monthly(),
                "is_free_tier": usage.is_free_tier(),
                "include_byok_in_limit": usage.include_byok_in_limit(),
            })),
            unavailable: None,
        }
    }

    fn unavailable(account: &GatewayAccount, reason: &str) -> AdminResponse {
        AdminResponse::Allowance {
            account: account.clone(),
            observed_at_unix_ms: None,
            view: None,
            unavailable: bounded(reason),
        }
    }
}

#[cfg(not(feature = "openrouter"))]
mod openrouter {
    use super::{AdminResponse, GatewayAccount, PlakPaths, not_observed};

    pub(super) async fn read(_home: &PlakPaths, account: &GatewayAccount) -> AdminResponse {
        not_observed(account)
    }
}
