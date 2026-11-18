//! The seam a host route's custody context will cross.
//!
//! Route constructors do not accept a relayed client and custodian yet, so a
//! host that names a gateway still refuses to open the local store. Replacing
//! [`open_for_host`] is the only change that starts host dispatch.

use crate::product_config::ProductConfig;

/// Refuses host dispatch through the gateway.
///
/// `config` is the home that named the gateway. The relay client and custodian
/// a route will take are not built here.
pub(crate) fn open_for_host(config: &ProductConfig) -> Result<(), String> {
    let _ = config.gateway_address.as_deref();
    Err(
        "this build's host does not dispatch model traffic through the credential gateway"
            .to_owned(),
    )
}
