//! Allowance reads return the held account's usage observation. API-key kinds
//! other than OpenRouter have no provider allowance endpoint.

use plakous::gateway::protocol::admin::AdminResponse;
use plakous::gateway::protocol::{BoundedText, GatewayAccount};
use plakous::model::{SubscriptionUsageRead, SubscriptionUsageUnavailable, Timestamp};
use plakous_host_protocol::views::presentation::SessionLedgerView;
use plakous_hosted::subscription_usage::{VerticalObservation, project_limits};

use crate::home::paths::PlakPaths;

pub(super) async fn read(home: &PlakPaths, account: &GatewayAccount) -> AdminResponse {
    if account.kind() == "openrouter" {
        return openrouter::read(home, account).await;
    }
    not_observed(account)
}

pub(super) fn not_observed(account: &GatewayAccount) -> AdminResponse {
    AdminResponse::Allowance {
        account: account.clone(),
        observed_at_unix_ms: None,
        view: None,
        unavailable: bounded("not-observed"),
    }
}

pub(super) fn subscription(account: &GatewayAccount, read: SubscriptionUsageRead) -> AdminResponse {
    let (observed_at, view, unavailable) = match read {
        SubscriptionUsageRead::Snapshot(ref snapshot) => {
            let observed_at = unix_ms(snapshot.observed_at());
            let Some(vertical) =
                crate::commands::usage::usage_vertical_for_account(account.as_str())
            else {
                return not_observed(account);
            };
            let view = project_limits(
                [(vertical, VerticalObservation::Native(read))],
                SessionLedgerView::default(),
            )
            .verticals
            .into_iter()
            .find(|view| view.vertical.account() == account.as_str())
            .and_then(|view| serde_json::to_value(view).ok());
            let unavailable = view.is_none().then(|| bounded("unavailable")).flatten();
            (observed_at, view, unavailable)
        }
        SubscriptionUsageRead::Unavailable {
            observed_at,
            reason,
        } => (
            unix_ms(observed_at),
            None,
            bounded(unavailable_reason(reason)),
        ),
    };
    AdminResponse::Allowance {
        account: account.clone(),
        observed_at_unix_ms: Some(observed_at),
        view,
        unavailable,
    }
}

fn unavailable_reason(reason: SubscriptionUsageUnavailable) -> &'static str {
    use SubscriptionUsageUnavailable as U;
    use plakous::model::SubscriptionUsageCredentialState as C;
    match reason {
        U::NotConnected => "not-connected",
        U::CredentialState(state) => match state {
            C::RefreshRequired => "refresh-required",
            C::Quarantined => "quarantined",
            C::Revoked => "revoked",
            C::LoginRequired => "login-required",
            C::WrittenByNewerBuild => "written-by-newer-build",
        },
        U::Busy => "busy",
        U::LocalState => "local-state",
        U::Unauthorized => "unauthorized",
        U::RateLimited => "rate-limited",
        U::ProviderUnavailable => "provider-unavailable",
        U::Transport(_) => "transport",
        U::InvalidResponse => "invalid-response",
        U::Stale => "stale",
        U::AccountChanged => "account-changed",
        U::Unqualified => "not-observed",
    }
}

fn unix_ms(at: Timestamp) -> u64 {
    at.seconds()
        .saturating_mul(1000)
        .saturating_add(u64::from(at.nanoseconds()) / 1_000_000)
}

fn bounded(text: &str) -> Option<BoundedText<64>> {
    BoundedText::new(text.to_owned()).ok()
}

mod openrouter {
    use std::sync::Arc;

    use super::{GatewayAccount, PlakPaths, bounded, not_observed, unix_ms};
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
        AdminResponse::Allowance {
            account: account.clone(),
            observed_at_unix_ms: Some(unix_ms(usage.observed_at())),
            view: Some(allowance_view(usage)),
            unavailable: None,
        }
    }

    fn allowance_view(usage: &plakous::openrouter::OpenRouterKeyUsage) -> serde_json::Value {
        let mut view = serde_json::Map::new();
        view.insert("limit".to_owned(), json_f64(usage.limit()));
        view.insert(
            "limit_remaining".to_owned(),
            json_f64(usage.limit_remaining()),
        );
        view.insert("limit_reset".to_owned(), json_text(usage.limit_reset()));
        view.insert("usage".to_owned(), json_f64(usage.usage()));
        view.insert("usage_daily".to_owned(), json_f64(usage.usage_daily()));
        view.insert("usage_weekly".to_owned(), json_f64(usage.usage_weekly()));
        view.insert("usage_monthly".to_owned(), json_f64(usage.usage_monthly()));
        view.insert("is_free_tier".to_owned(), json_bool(usage.is_free_tier()));
        view.insert(
            "include_byok_in_limit".to_owned(),
            json_bool(usage.include_byok_in_limit()),
        );
        serde_json::Value::Object(view)
    }

    fn json_f64(value: Option<f64>) -> serde_json::Value {
        value.map_or(serde_json::Value::Null, |number| {
            serde_json::Number::from_f64(number)
                .map_or(serde_json::Value::Null, serde_json::Value::Number)
        })
    }

    fn json_bool(value: Option<bool>) -> serde_json::Value {
        value.map_or(serde_json::Value::Null, serde_json::Value::Bool)
    }

    fn json_text(value: Option<&str>) -> serde_json::Value {
        value.map_or(serde_json::Value::Null, |text| {
            serde_json::Value::String(text.to_owned())
        })
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
