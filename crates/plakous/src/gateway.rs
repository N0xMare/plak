//! The credential gateway seam: custody, the protocol, the relay client and
//! the remote custodian, each provider's upstream pins, and the relay server
//! core over local custody (`server`).

pub use plakous_auth::custody;
pub use plakous_gateway_protocol as protocol;
pub use plakous_http::relay::{
    GatewayEndpoint, InMemoryGateway, MAX_FENCE_REPREPARATIONS, RelayCallError, RelayClient,
    custodied_placeholder,
};

mod client;
pub mod server;

pub use client::{CustodiedKeySource, GatewayCustody};

/// Each compiled provider's exact upstream pins and credential header names.
pub mod pins {
    /// Anthropic Platform and Claude subscription rows.
    #[cfg(any(feature = "anthropic", feature = "claude"))]
    pub mod anthropic {
        pub use plakous_anthropic::{
            ANTHROPIC_API_CREDENTIAL_HEADERS, ANTHROPIC_API_UPSTREAM_PINS,
            CLAUDE_CREDENTIAL_HEADERS, CLAUDE_UPSTREAM_PINS,
        };
    }
    /// OpenAI Platform and Codex subscription rows.
    #[cfg(any(feature = "openai", feature = "codex"))]
    pub mod openai {
        pub use plakous_openai::{
            CODEX_CREDENTIAL_HEADERS, CODEX_UPSTREAM_PINS, OPENAI_API_CREDENTIAL_HEADERS,
            OPENAI_API_UPSTREAM_PINS,
        };
    }
    /// OpenRouter rows.
    #[cfg(feature = "openrouter")]
    pub mod openrouter {
        pub use plakous_openrouter::{OPENROUTER_CREDENTIAL_HEADERS, OPENROUTER_UPSTREAM_PINS};
    }
    /// xAI Platform and SuperGrok subscription rows.
    #[cfg(any(feature = "xai", feature = "grok"))]
    pub mod xai {
        pub use plakous_xai::{
            SUPERGROK_CREDENTIAL_HEADERS, SUPERGROK_UPSTREAM_PINS, XAI_API_CREDENTIAL_HEADERS,
            XAI_API_UPSTREAM_PINS,
        };
    }
}
