//! The repository-only dial override for a direct client (`fault-injection`):
//! absent from product builds.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use super::hooks::{HookFuture, Relay};
use super::test_hop::HopFaults;
use crate::{
    Client, ErrorKind, ExchangeControl, Failure, Limits, Origin, Phase, Request, Response,
    TransmissionFact,
};

struct DialOverride {
    logical: Origin,
    dial: Origin,
    upstream: Client,
}

impl Relay for DialOverride {
    fn execute<'a>(
        &'a self,
        limits: &'a Limits,
        request: Request,
        control: ExchangeControl,
        // A direct client stand-in: `upstream` applies its own plan.
        _faults: HopFaults,
    ) -> HookFuture<'a, Result<Response, Failure>> {
        Box::pin(async move {
            let (origin, method, path, headers, body) = request.into_parts();
            let origin = if origin == self.logical {
                self.dial.clone()
            } else {
                origin
            };
            let request = Request::new(
                origin,
                method,
                path.as_str(),
                headers.into_inner(),
                body,
                limits,
            )
            .map_err(|_| {
                Failure::new(
                    Phase::RequestWrite,
                    TransmissionFact::NotStarted,
                    ErrorKind::Internal,
                )
            })?;
            self.upstream.execute(request, control).await
        })
    }
}

impl Client {
    /// Repository tests only: a client sending through `upstream` that dials
    /// `logical` at `dial` instead, so a route bound to a production origin
    /// reaches a loopback fixture exactly as a relay with the same override.
    #[must_use]
    pub fn with_test_dial_override(&self, logical: Origin, dial: Origin, upstream: Client) -> Self {
        self.with_relay_hook(Arc::new(DialOverride {
            logical,
            dial,
            upstream,
        }))
    }
}

/// Lets the real gateway process drive the offline OAuth fixture without
/// changing the endpoints the provider workflows validate.
pub(super) fn gateway_oauth_fixture(
    origin: &Origin,
    path: &str,
) -> Result<Option<Origin>, Failure> {
    if !matches!(
        (origin.host(), path),
        (
            "auth.openai.com",
            "/api/accounts/deviceauth/usercode" | "/api/accounts/deviceauth/token"
        ) | ("platform.claude.com", "/v1/oauth/token")
    ) {
        return Ok(None);
    }
    let Some(port) = std::env::var_os("PLAK_TEST_GATEWAY_OAUTH_PORT") else {
        return Ok(None);
    };
    port.to_str()
        .and_then(|value| value.parse().ok())
        .and_then(|port| Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), port).ok())
        .map(Some)
        .ok_or_else(|| {
            Failure::new(
                Phase::Resolution,
                TransmissionFact::NotStarted,
                ErrorKind::Internal,
            )
        })
}
