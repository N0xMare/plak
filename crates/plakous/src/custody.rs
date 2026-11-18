//! Who holds route credentials (spec 01 §7.1): this process, under local
//! custody, or a credential gateway.

use std::sync::Arc;

use plakous_auth::custody::CredentialCustodian;
use plakous_http::RouteTransport;

/// Who holds route credentials. Local custody is the default and dispatches
/// exactly as a route does on its own; gateway custody
/// (`plakous::gateway::GatewayCustody::custody`) relays every route exchange
/// through a credential gateway that fills the credential fields.
#[derive(Clone, Default)]
pub struct Custody {
    transport: RouteTransport,
    custodian: Option<Arc<dyn CredentialCustodian>>,
}

impl std::fmt::Debug for Custody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Custody")
            .field("transport", &self.transport)
            .field("custodian", &self.custodian.is_some())
            .finish()
    }
}

impl Custody {
    /// This process holds the credentials.
    #[must_use]
    pub const fn local() -> Self {
        Self {
            transport: RouteTransport::direct(),
            custodian: None,
        }
    }

    /// Custody whose routes send through `transport` (a gateway's,
    /// `plakous::gateway::GatewayCustody::custody`).
    #[must_use]
    pub const fn relayed(transport: RouteTransport) -> Self {
        Self {
            transport,
            custodian: None,
        }
    }

    /// Custody whose routes send through `transport` and whose subscription
    /// logins `custodian` holds: facts, refresh and custodied fields.
    #[must_use]
    pub fn gateway(transport: RouteTransport, custodian: Arc<dyn CredentialCustodian>) -> Self {
        Self {
            transport,
            custodian: Some(custodian),
        }
    }

    /// Who holds the subscription logins, under gateway custody.
    #[must_use]
    pub const fn custodian(&self) -> Option<&Arc<dyn CredentialCustodian>> {
        self.custodian.as_ref()
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
