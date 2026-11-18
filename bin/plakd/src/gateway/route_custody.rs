//! Host-side custody through a configured credential gateway.
//!
//! API-key routes take this client. Subscription routes stay in-process until
//! their constructors take custody.

use plakous::gateway::{GatewayCustody, GatewayEndpoint, SensitiveBytes};

use super::address::GatewayAddress;
use super::route::AccountRoute;
use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;

/// The gateway a host or credential provider should use, when `[gateway]
/// address` is set. Unset is local custody.
pub(crate) fn connect(home: &PlakPaths) -> Result<Option<GatewayCustody>, String> {
    let config = ProductConfig::load(home).unwrap_or_else(|_| ProductConfig::default_product());
    let Some(text) = config.gateway_address.as_deref() else {
        return Ok(None);
    };
    let address = super::address::parse(text, config.gateway_allow_non_tailnet)
        .map_err(|error| format!("gateway.address: {error}"))?;
    let key = super::cli_cmd::caller_key(home, &AccountRoute::Configured(address.clone()))?;
    let endpoint = match address {
        GatewayAddress::Unix(path) => GatewayEndpoint::Unix(path),
        GatewayAddress::Tcp(addr) => GatewayEndpoint::Tcp(addr),
    };
    let key = key.map(|text| SensitiveBytes::new(text.into_bytes()));
    GatewayCustody::connect(endpoint, key, credential_headers)
        .map(Some)
        .map_err(|error| format!("credential gateway: {error}"))
}

fn credential_headers(kind: &str) -> Option<&'static [&'static str]> {
    openai::headers(kind)
        .or_else(|| anthropic::headers(kind))
        .or_else(|| xai::headers(kind))
        .or_else(|| openrouter::headers(kind))
        .or_else(|| codex::headers(kind))
        .or_else(|| claude::headers(kind))
        .or_else(|| grok::headers(kind))
}

#[cfg(feature = "openai")]
mod openai {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "openai").then_some(plakous::gateway::pins::openai::OPENAI_API_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "openai"))]
mod openai {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "anthropic")]
mod anthropic {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "anthropic")
            .then_some(plakous::gateway::pins::anthropic::ANTHROPIC_API_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "anthropic"))]
mod anthropic {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "xai-api")]
mod xai {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "xai-api").then_some(plakous::gateway::pins::xai::XAI_API_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "xai-api"))]
mod xai {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "openrouter")]
mod openrouter {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "openrouter")
            .then_some(plakous::gateway::pins::openrouter::OPENROUTER_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "openrouter"))]
mod openrouter {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "codex")]
mod codex {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "codex").then_some(plakous::gateway::pins::openai::CODEX_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "codex"))]
mod codex {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "claude")]
mod claude {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "claude").then_some(plakous::gateway::pins::anthropic::CLAUDE_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "claude"))]
mod claude {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(feature = "grok")]
mod grok {
    pub(super) fn headers(kind: &str) -> Option<&'static [&'static str]> {
        (kind == "grok").then_some(plakous::gateway::pins::xai::SUPERGROK_CREDENTIAL_HEADERS)
    }
}

#[cfg(not(feature = "grok"))]
mod grok {
    pub(super) fn headers(_kind: &str) -> Option<&'static [&'static str]> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_address_stays_local() {
        let directory = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(directory.path().join("home"));
        assert!(connect(&paths).unwrap().is_none());
    }
}
