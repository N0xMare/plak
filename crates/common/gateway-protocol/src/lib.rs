//! Credential gateway protocol, version 1: what a Plakous caller and the
//! credential gateway that holds its credentials say to each other (spec 01
//! §7.5). This crate does no I/O.
//!
//! The carrier is HTTP/1.1, one exchange per connection use, with the
//! protocol version in the path ([`Endpoint::path`]). A gateway serves its
//! own version and the immediately previous one; [`negotiate`] picks the
//! highest version both ends speak from the gateway's [`VersionsReply`].
//!
//! - [`envelope`]: the relay request, one upstream exchange made with a
//!   custodied credential.
//! - [`frame`]: the relay response frames and their bounded decoder.
//! - [`refusal`]: the gateway's own refusals, decided before any upstream
//!   byte, never confused with an upstream status.
//! - [`facts`]: the non-secret account facts callers revalidate against, and
//!   the facts and refresh calls.
//! - [`admin`]: account administration calls.
//! - [`scope`]: caller keys and scopes.
//! - [`call`]: the JSON body codec for every call but relay.

pub mod admin;
pub mod call;
pub mod envelope;
pub mod facts;
pub mod frame;
pub mod header;
pub mod refusal;
pub mod scope;

mod account;
mod secret;
mod text;

pub use account::GatewayAccount;
pub use secret::{SecretText, SecretTextTooLarge};
pub use text::{BoundedText, HexDigest, TextError, UuidText};

/// The newest protocol version this crate speaks.
pub const PROTOCOL_VERSION: u16 = 1;

/// Every version this crate speaks, ascending: its own and, once there is
/// one, the immediately previous version.
pub const SUPPORTED_VERSIONS: &[u16] = &[1];

/// The request header carrying the caller key.
pub const KEY_HEADER: &str = "plak-gateway-key";
/// The response header naming a gateway refusal's code. Its presence is what
/// marks a non-200 answer as the gateway's own.
pub const ERROR_HEADER: &str = "plak-gateway-error";
/// The relay request body's media type.
pub const RELAY_REQUEST_CONTENT_TYPE: &str = "application/vnd.plak.relay";
/// The relay response body's media type.
pub const RELAY_FRAMES_CONTENT_TYPE: &str = "application/vnd.plak.relay-frames";
/// The unversioned path that answers a [`VersionsReply`].
pub const VERSIONS_PATH: &str = "/versions";

/// The highest version in both lists.
#[must_use]
pub fn negotiate(ours: &[u16], theirs: &[u16]) -> Option<u16> {
    ours.iter()
        .copied()
        .filter(|version| theirs.contains(version))
        .max()
}

/// The answer to `GET /versions`.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionsReply {
    /// The versions the gateway serves, ascending.
    pub versions: Vec<u16>,
    /// The gateway process instance; it changes when the gateway restarts.
    pub instance: UuidText,
}

/// A versioned gateway endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endpoint {
    /// `POST /v<N>/relay`: one upstream exchange ([`envelope`], [`frame`]).
    Relay,
    /// `POST /v<N>/facts`: account facts ([`facts::FactsRequest`]).
    Facts,
    /// `POST /v<N>/refresh`: one single-writer refresh ([`facts::RefreshRequest`]).
    Refresh,
    /// `POST /v<N>/admin`: one admin call ([`admin::AdminRequest`]).
    Admin,
}

impl Endpoint {
    /// Every endpoint.
    pub const ALL: [Self; 4] = [Self::Relay, Self::Facts, Self::Refresh, Self::Admin];

    const fn name(self) -> &'static str {
        match self {
            Self::Relay => "relay",
            Self::Facts => "facts",
            Self::Refresh => "refresh",
            Self::Admin => "admin",
        }
    }

    /// The endpoint's path under `version`.
    #[must_use]
    pub fn path(self, version: u16) -> String {
        format!("/v{version}/{}", self.name())
    }

    /// The version and endpoint a request path names. The version is decimal
    /// without leading zeros; the caller still checks it is served.
    #[must_use]
    pub fn parse(path: &str) -> Option<(u16, Self)> {
        let (version, name) = path.strip_prefix("/v")?.split_once('/')?;
        if version.is_empty()
            || version.starts_with('0')
            || !version.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let version = version.parse().ok()?;
        let endpoint = Self::ALL
            .into_iter()
            .find(|endpoint| endpoint.name() == name)?;
        Some((version, endpoint))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod pinned_tests;
