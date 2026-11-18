//! Who holds route credentials (spec 01 §7.1): this process, under local
//! custody, or a credential gateway.

use plakous_http::RouteTransport;

/// Who holds route credentials. Local custody is the default and dispatches
/// exactly as a route does on its own; gateway custody
/// (`plakous::gateway::GatewayCustody::custody`) relays every route exchange
/// through a credential gateway that fills the credential fields.
#[derive(Clone, Debug, Default)]
pub struct Custody {
    transport: RouteTransport,
}

impl Custody {
    /// This process holds the credentials.
    #[must_use]
    pub const fn local() -> Self {
        Self {
            transport: RouteTransport::direct(),
        }
    }

    /// Custody whose routes send through `transport` (a gateway's,
    /// `plakous::gateway::GatewayCustody::custody`).
    #[must_use]
    pub const fn relayed(transport: RouteTransport) -> Self {
        Self { transport }
    }

    /// Whether this process holds the credentials.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        self.transport.is_direct()
    }

    /// Where route clients send their exchanges under this custody.
    #[must_use]
    pub fn transport(&self) -> RouteTransport {
        self.transport.clone()
    }
}
