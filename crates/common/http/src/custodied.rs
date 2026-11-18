//! Custodied credential placeholders.
//!
//! A credential a gateway holds is represented in a prepared request by a
//! reserved header value at the credential's exact header position. A
//! relaying client sends that entry as `[name, null]` for the gateway to
//! fill; a direct exchange refuses any request carrying one, so a
//! placeholder never reaches a provider. The value starts with byte `0xff`,
//! which no credential this workspace lowers begins with, and names the
//! account and the fence the request was prepared against.

pub(crate) const PREFIX: &[u8] = b"\xffplak-custodied ";

/// Whether `value` is a custodied placeholder.
pub(crate) fn is_placeholder(value: &[u8]) -> bool {
    value.starts_with(PREFIX)
}
