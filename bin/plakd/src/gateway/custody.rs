//! Local custody of the home's API keys and subscription logins.

use std::sync::Arc;

use plakous::api_key::{ApiKey, CredentialProviderTag};
use plakous::gateway::custody::CustodySlot;
use plakous::gateway::custody::{
    CustodyError, KeyHeaderForm, KeyInjection, KeySource as CustodyKeySource, LocalKey,
    LocalKeyCustodian, LocalKeyStore, UpstreamPin,
};
use plakous::gateway::server::LocalCustody;
use plakous::subscriptions::{NativeSubscriptions, SubscriptionPaths};

use crate::home::paths::PlakPaths;
use crate::home::secrets::{self, KeySource};

/// One API-key kind the gateway relays, with its pin table.
struct CustodiedKeyKind {
    name: &'static str,
    provider: CredentialProviderTag,
    header: &'static str,
    bearer: bool,
    pins: &'static [UpstreamPin],
}

/// API-key custody over `home`'s key files and environment, for gateway
/// process `instance`.
pub(crate) fn local_custody(
    home: &PlakPaths,
    instance: plakous::gateway::protocol::UuidText,
    state_root: &std::path::Path,
) -> LocalCustody {
    let store = Arc::new(HomeKeyStore { home: home.clone() });
    let custodian: Arc<dyn plakous::gateway::custody::CredentialCustodian> =
        Arc::new(LocalKeyCustodian::new(store, injection_for));
    let mut custody = LocalCustody::new(instance);
    for kind in custodied_key_kinds() {
        custody = custody.with_kind(kind.name, Arc::clone(&custodian), kind.pins);
    }
    register_subscriptions(custody, state_root)
}

/// Whether relay and facts for `account` come from [`local_custody`] rather
/// than the account bridge.
pub(crate) fn custodied(account: &plakous::gateway::protocol::GatewayAccount) -> bool {
    custodied_api_key(account) || subscription_enabled(account.kind())
}

/// Whether `account` is an API-key kind this process relays.
pub(crate) fn custodied_api_key(account: &plakous::gateway::protocol::GatewayAccount) -> bool {
    custodied_key_kinds()
        .iter()
        .any(|kind| kind.name == account.kind())
}

fn subscription_enabled(kind: &str) -> bool {
    codex_login::serves(kind) || claude_login::serves(kind) || grok_login::serves(kind)
}

fn register_subscriptions(custody: LocalCustody, state_root: &std::path::Path) -> LocalCustody {
    let Some(subscriptions) = open_subscriptions(state_root) else {
        return custody;
    };
    let custody = codex_login::register(custody, &subscriptions);
    let custody = claude_login::register(custody, &subscriptions);
    grok_login::register(custody, &subscriptions)
}

fn open_subscriptions(state_root: &std::path::Path) -> Option<NativeSubscriptions> {
    SubscriptionPaths::from_xdg_state_home(state_root)
        .ok()
        .map(NativeSubscriptions::new)
}

fn injection_for(kind: &str) -> Option<KeyInjection> {
    let kind = custodied_key_kinds()
        .into_iter()
        .find(|item| item.name == kind)?;
    Some(KeyInjection {
        provider: kind.provider,
        header: kind.header,
        form: if kind.bearer {
            KeyHeaderForm::Bearer
        } else {
            KeyHeaderForm::Raw
        },
    })
}

fn custodied_key_kinds() -> Vec<CustodiedKeyKind> {
    let mut kinds = Vec::new();
    openai_kind::push(&mut kinds);
    anthropic_kind::push(&mut kinds);
    xai_kind::push(&mut kinds);
    openrouter_kind::push(&mut kinds);
    kinds
}

#[cfg(feature = "openai")]
mod openai_kind {
    pub(super) fn push(kinds: &mut Vec<super::CustodiedKeyKind>) {
        kinds.push(super::CustodiedKeyKind {
            name: "openai",
            provider: plakous::api_key::CredentialProviderTag::OpenAiPlatform,
            header: plakous::gateway::pins::openai::OPENAI_API_CREDENTIAL_HEADERS[0],
            bearer: true,
            pins: plakous::gateway::pins::openai::OPENAI_API_UPSTREAM_PINS,
        });
    }
}

#[cfg(not(feature = "openai"))]
mod openai_kind {
    pub(super) fn push(_kinds: &mut Vec<super::CustodiedKeyKind>) {}
}

#[cfg(feature = "anthropic")]
mod anthropic_kind {
    pub(super) fn push(kinds: &mut Vec<super::CustodiedKeyKind>) {
        kinds.push(super::CustodiedKeyKind {
            name: "anthropic",
            provider: plakous::api_key::CredentialProviderTag::AnthropicPlatform,
            header: plakous::gateway::pins::anthropic::ANTHROPIC_API_CREDENTIAL_HEADERS[0],
            bearer: false,
            pins: plakous::gateway::pins::anthropic::ANTHROPIC_API_UPSTREAM_PINS,
        });
    }
}

#[cfg(not(feature = "anthropic"))]
mod anthropic_kind {
    pub(super) fn push(_kinds: &mut Vec<super::CustodiedKeyKind>) {}
}

#[cfg(feature = "xai-api")]
mod xai_kind {
    pub(super) fn push(kinds: &mut Vec<super::CustodiedKeyKind>) {
        kinds.push(super::CustodiedKeyKind {
            name: "xai-api",
            provider: plakous::api_key::CredentialProviderTag::XaiPlatform,
            header: plakous::gateway::pins::xai::XAI_API_CREDENTIAL_HEADERS[0],
            bearer: true,
            pins: plakous::gateway::pins::xai::XAI_API_UPSTREAM_PINS,
        });
    }
}

#[cfg(not(feature = "xai-api"))]
mod xai_kind {
    pub(super) fn push(_kinds: &mut Vec<super::CustodiedKeyKind>) {}
}

#[cfg(feature = "openrouter")]
mod openrouter_kind {
    pub(super) fn push(kinds: &mut Vec<super::CustodiedKeyKind>) {
        kinds.push(super::CustodiedKeyKind {
            name: "openrouter",
            provider: plakous::api_key::CredentialProviderTag::OpenRouterPlatform,
            header: plakous::gateway::pins::openrouter::OPENROUTER_CREDENTIAL_HEADERS[0],
            bearer: true,
            pins: plakous::gateway::pins::openrouter::OPENROUTER_UPSTREAM_PINS,
        });
    }
}

#[cfg(not(feature = "openrouter"))]
mod openrouter_kind {
    pub(super) fn push(_kinds: &mut Vec<super::CustodiedKeyKind>) {}
}

struct HomeKeyStore {
    home: PlakPaths,
}

impl LocalKeyStore for HomeKeyStore {
    fn key(&self, slot: &CustodySlot) -> Result<Option<LocalKey>, CustodyError> {
        let account = slot.as_str();
        if !secrets::is_api_key_account(account) {
            return Err(CustodyError::UnknownAccount);
        }
        let status = secrets::key_status(&self.home, account).map_err(|_| CustodyError::Store)?;
        let source = match status.source {
            KeySource::Missing => return Ok(None),
            KeySource::Env => CustodyKeySource::Environment,
            KeySource::File => CustodyKeySource::File,
        };
        let value = secrets::resolve_api_key(&self.home, account)
            .map_err(|_| CustodyError::Store)?
            .ok_or(CustodyError::Store)?;
        let key = ApiKey::new(value).map_err(|_| CustodyError::Store)?;
        Ok(Some(LocalKey {
            key,
            source,
            env_var: status.env_var.map(str::to_owned),
        }))
    }
}

#[cfg(feature = "codex")]
mod codex_login {
    use std::sync::Arc;

    use plakous::gateway::server::LocalCustody;
    use plakous::subscriptions::custody::SubscriptionCustodian;
    use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

    pub(super) fn serves(kind: &str) -> bool {
        kind == "codex"
    }

    pub(super) fn register(
        custody: LocalCustody,
        subscriptions: &NativeSubscriptions,
    ) -> LocalCustody {
        let provider = NativeProvider::Codex;
        let subscriptions =
            crate::test_hooks::gateway_review_clock(subscriptions.clone(), provider);
        let current = SubscriptionCustodian::new(subscriptions, provider);
        let v1 = current.for_gateway_v1();
        custody.with_kind_versions(
            "codex",
            Arc::new(current),
            Some(Arc::new(v1)),
            plakous::gateway::pins::openai::CODEX_UPSTREAM_PINS,
        )
    }
}

#[cfg(not(feature = "codex"))]
mod codex_login {
    use plakous::gateway::server::LocalCustody;
    use plakous::subscriptions::NativeSubscriptions;

    pub(super) fn serves(_kind: &str) -> bool {
        false
    }

    pub(super) fn register(
        custody: LocalCustody,
        _subscriptions: &NativeSubscriptions,
    ) -> LocalCustody {
        custody
    }
}

#[cfg(feature = "claude")]
mod claude_login {
    use std::sync::Arc;

    use plakous::gateway::server::LocalCustody;
    use plakous::subscriptions::custody::SubscriptionCustodian;
    use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

    pub(super) fn serves(kind: &str) -> bool {
        kind == "claude"
    }

    pub(super) fn register(
        custody: LocalCustody,
        subscriptions: &NativeSubscriptions,
    ) -> LocalCustody {
        let provider = NativeProvider::Claude;
        let subscriptions =
            crate::test_hooks::gateway_review_clock(subscriptions.clone(), provider);
        custody.with_kind(
            "claude",
            Arc::new(SubscriptionCustodian::new(subscriptions, provider)),
            plakous::gateway::pins::anthropic::CLAUDE_UPSTREAM_PINS,
        )
    }
}

#[cfg(not(feature = "claude"))]
mod claude_login {
    use plakous::gateway::server::LocalCustody;
    use plakous::subscriptions::NativeSubscriptions;

    pub(super) fn serves(_kind: &str) -> bool {
        false
    }

    pub(super) fn register(
        custody: LocalCustody,
        _subscriptions: &NativeSubscriptions,
    ) -> LocalCustody {
        custody
    }
}

#[cfg(feature = "grok")]
mod grok_login {
    use std::sync::Arc;

    use plakous::gateway::server::LocalCustody;
    use plakous::subscriptions::custody::SubscriptionCustodian;
    use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

    pub(super) fn serves(kind: &str) -> bool {
        kind == "grok"
    }

    pub(super) fn register(
        custody: LocalCustody,
        subscriptions: &NativeSubscriptions,
    ) -> LocalCustody {
        let provider = NativeProvider::Xai;
        let subscriptions =
            crate::test_hooks::gateway_review_clock(subscriptions.clone(), provider);
        let current = SubscriptionCustodian::new(subscriptions, provider);
        let v1 = current.for_gateway_v1();
        custody.with_kind_versions(
            "grok",
            Arc::new(current),
            Some(Arc::new(v1)),
            plakous::gateway::pins::xai::SUPERGROK_UPSTREAM_PINS,
        )
    }
}

#[cfg(not(feature = "grok"))]
mod grok_login {
    use plakous::gateway::server::LocalCustody;
    use plakous::subscriptions::NativeSubscriptions;

    pub(super) fn serves(_kind: &str) -> bool {
        false
    }

    pub(super) fn register(
        custody: LocalCustody,
        _subscriptions: &NativeSubscriptions,
    ) -> LocalCustody {
        custody
    }
}
