//! What `tailscale serve` forwards to a unix backend, and what of it the
//! gate trusts (spec 04 §2.10).
//!
//! Serve proxies to a unix socket as `http://localhost`: every request
//! arrives with `Host: localhost`, and the name the browser used in
//! `X-Forwarded-Host`, with `X-Forwarded-Proto: https` and the tailnet
//! source address in `X-Forwarded-For`. Serve's reverse proxy deletes any
//! `X-Forwarded-*` a client sent before it sets its own, so these values
//! are Serve's, and only on the private unix listener, where only Serve (or
//! this user) can connect.

use std::net::{IpAddr, SocketAddr, UdpSocket};

use hyper::header::{self, HeaderMap};

use super::auth::Refusal;

/// The one value of `name`, or `None` when it is absent, repeated or not
/// text.
fn single(headers: &HeaderMap, name: impl header::AsHeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    }
}

/// Admits exactly Serve's shape of a request for `authority`, from a
/// source that is not this machine.
///
/// # Errors
///
/// [`Refusal::Host`] for any other `Host`, forwarded host or protocol, and
/// [`Refusal::Source`] for a missing, repeated or local source.
pub(super) fn check_serve_request(headers: &HeaderMap, authority: &str) -> Result<(), Refusal> {
    if single(headers, header::HOST) != Some("localhost")
        || single(headers, "x-forwarded-host") != Some(authority)
        || single(headers, "x-forwarded-proto") != Some("https")
    {
        return Err(Refusal::Host);
    }
    let source = single(headers, "x-forwarded-for")
        .and_then(|value| value.parse::<IpAddr>().ok())
        .ok_or(Refusal::Source)?;
    if is_this_host(source) {
        return Err(Refusal::Source);
    }
    Ok(())
}

/// Whether `address` is this machine's own: loopback, unspecified,
/// link-local (no tailnet source is), or an address a local socket can bind
/// (which only an address of a local interface can be). Serve stamps the
/// serving node owner's login on a connection from the node itself, so a
/// request from this machine carries an identity that no device of that
/// login sent. A host configured to bind any address (`ip_nonlocal_bind`)
/// refuses every request: it fails closed. Under userspace networking the
/// node's tailnet addresses are on no local interface, so this check cannot
/// see them and the tag is the only control (spec 04 §2.10).
fn is_this_host(address: IpAddr) -> bool {
    let link_local = match address {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unicast_link_local(),
    };
    address.is_loopback()
        || address.is_unspecified()
        || link_local
        || UdpSocket::bind(SocketAddr::new(address, 0)).is_ok()
}
