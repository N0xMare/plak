//! The repository-only dial override for a direct client (`fault-injection`):
//! absent from product builds.

use std::sync::Arc;

use super::hooks::{HookFuture, Relay};
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
