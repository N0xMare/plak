//! Where a route's exchanges go: directly upstream, or through a credential
//! gateway's relay (spec 01 §7.1).

use std::sync::Arc;

use crate::Client;
use crate::transport::hooks::Relay;

/// How a route's client reaches upstream. The route still builds its own
/// client with its own limits, policy and identity; the transport decides
/// only whether its exchanges go directly upstream or through a credential
/// gateway, which then fills the custodied credential fields.
#[derive(Clone, Default)]
pub struct RouteTransport(pub(crate) Option<Arc<dyn Relay>>);

impl RouteTransport {
    /// Directly upstream: the route's client unchanged.
    #[must_use]
    pub const fn direct() -> Self {
        Self(None)
    }

    /// Whether exchanges go directly upstream.
    #[must_use]
    pub const fn is_direct(&self) -> bool {
        self.0.is_none()
    }

    /// `client` taking this transport.
    #[must_use]
    pub fn client(&self, client: Client) -> Client {
        match &self.0 {
            None => client,
            Some(relay) => client.with_relay_hook(Arc::clone(relay)),
        }
    }
}

impl std::fmt::Debug for RouteTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(if self.0.is_none() {
            "RouteTransport::Direct"
        } else {
            "RouteTransport::Relayed"
        })
    }
}
