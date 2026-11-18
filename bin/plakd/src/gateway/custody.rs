//! Local custody of the home's API keys. Subscription kinds are not registered
//! in this build; [`subscription_kind`] is the seam that will register them.

use std::sync::Arc;

use plakous::api_key::{ApiKey, CredentialProviderTag};
use plakous::gateway::custody::CustodySlot;
use plakous::gateway::custody::{
    CustodyError, KeyHeaderForm, KeyInjection, KeySource as CustodyKeySource, LocalKey,
    LocalKeyCustodian, LocalKeyStore, UpstreamPin,
};
use plakous::gateway::server::LocalCustody;

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
) -> LocalCustody {
    let store = Arc::new(HomeKeyStore { home: home.clone() });
    let custodian: Arc<dyn plakous::gateway::custody::CredentialCustodian> =
        Arc::new(LocalKeyCustodian::new(store, injection_for));
    let mut custody = LocalCustody::new(instance);
    for kind in custodied_key_kinds() {
        custody = custody.with_kind(kind.name, Arc::clone(&custodian), kind.pins);
    }
    register_subscriptions(custody)
}

/// Whether relay and facts for `account` come from [`local_custody`] rather
/// than the account bridge. Subscription accounts do not, yet.
pub(crate) fn custodied_api_key(account: &plakous::gateway::protocol::GatewayAccount) -> bool {
    custodied_key_kinds()
        .iter()
        .any(|kind| kind.name == account.kind())
}

/// The subscription custodian and pin table for `kind` (`codex`, `claude`,
/// `grok`). This build has none: route constructors do not take a custody
/// context yet, and those kinds stay on the account bridge.
pub(crate) fn subscription_kind(
    kind: &str,
) -> Option<(
    Arc<dyn plakous::gateway::custody::CredentialCustodian>,
    &'static [UpstreamPin],
)> {
    let _ = kind;
    None
}

fn register_subscriptions(mut custody: LocalCustody) -> LocalCustody {
    for kind in ["codex", "claude", "grok"] {
        if let Some((custodian, pins)) = subscription_kind(kind) {
            custody = custody.with_kind(kind, custodian, pins);
        }
    }
    custody
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
