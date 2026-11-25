//! Caller authentication. Before it succeeds, every failure is the uniform
//! 401: no account, scope or parse detail.

use std::net::IpAddr;

#[cfg(test)]
use plakous::gateway::protocol::KEY_HEADER;
use plakous::gateway::protocol::refusal::GatewayRefusal;
use plakous::gateway::protocol::scope::{CallerId, CallerKey};

use super::callers::{CallerRecord, Callers};
#[cfg(test)]
use super::http::Request;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Principal {
    /// A same-UID peer on the gateway's own socket, with no caller key.
    LocalAdmin,
    Caller(CallerId),
}

pub(crate) struct Peer {
    pub uid_matches: bool,
    pub address: Option<IpAddr>,
}

/// Unix with a matching uid and no key is local-admin. TCP always needs a
/// key whose source network contains the peer. A presented key that does not
/// match is unauthenticated, never a fall-through to local-admin.
pub(crate) fn authenticate_key<'a>(
    callers: &'a Callers,
    peer: &Peer,
    presented: Option<&str>,
) -> Result<(Principal, Option<&'a CallerRecord>), GatewayRefusal> {
    if peer.uid_matches && presented.is_none() && peer.address.is_none() {
        return Ok((Principal::LocalAdmin, None));
    }
    let Some(text) = presented else {
        return Err(GatewayRefusal::Unauthenticated {});
    };
    let key = CallerKey::parse(text).map_err(|_| GatewayRefusal::Unauthenticated {})?;
    let Some(record) = callers.authenticate(&key) else {
        return Err(GatewayRefusal::Unauthenticated {});
    };
    if let Some(address) = peer.address {
        if !record.sources.iter().any(|source| source.contains(address)) {
            return Err(GatewayRefusal::Unauthenticated {});
        }
    } else if !peer.uid_matches {
        return Err(GatewayRefusal::Unauthenticated {});
    }
    Ok((Principal::Caller(record.caller.clone()), Some(record)))
}

#[cfg(test)]
pub(crate) fn authenticate<'a>(
    callers: &'a Callers,
    peer: &Peer,
    request: &Request,
) -> Result<(Principal, Option<&'a CallerRecord>), GatewayRefusal> {
    let presented = request.headers.get(KEY_HEADER).map(String::as_str);
    authenticate_key(callers, peer, presented)
}

impl Principal {
    pub(crate) fn audit_name(&self) -> String {
        match self {
            Self::LocalAdmin => "local-admin".to_owned(),
            Self::Caller(id) => id.as_str().to_owned(),
        }
    }

    pub(crate) fn is_local_admin(&self) -> bool {
        matches!(self, Self::LocalAdmin)
    }
}
