//! Gateway addresses: a unix socket path or one TCP address, limited to
//! loopback and the tailnet unless the operator opts out. A wildcard bind is
//! never accepted.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

/// `100.64.0.0/10`.
const TAILNET_V4: u32 = 0x6440_0000;
const TAILNET_V4_MASK: u32 = 0xffc0_0000;
/// `fd7a:115c:a1e0::/48`.
const TAILNET_V6: u128 = 0xfd7a_115c_a1e0;
const TAILNET_V6_SHIFT: u32 = 80;

/// Where a gateway client connects, or where the service listens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GatewayAddress {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

impl GatewayAddress {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Unix(path) => format!("unix:{}", path.display()),
            Self::Tcp(addr) => format!("tcp:{addr}"),
        }
    }
}

/// Why an address was refused. The text names the rule, never a credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AddressError(pub(crate) &'static str);

impl std::fmt::Display for AddressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

/// Parses `unix:<absolute path>` or `tcp:<ip>:<port>`.
pub(crate) fn parse(text: &str, allow_non_tailnet: bool) -> Result<GatewayAddress, AddressError> {
    if let Some(path) = text.strip_prefix("unix:") {
        return unix_path(path).map(GatewayAddress::Unix);
    }
    let Some(addr) = text.strip_prefix("tcp:") else {
        return Err(AddressError(
            "gateway address must be unix:<absolute path> or tcp:<ip>:<port>",
        ));
    };
    let socket: SocketAddr = addr
        .parse()
        .map_err(|_| AddressError("tcp gateway address must be <ip>:<port>"))?;
    admit_ip(socket.ip(), allow_non_tailnet)?;
    Ok(GatewayAddress::Tcp(socket))
}

fn unix_path(path: &str) -> Result<PathBuf, AddressError> {
    let path = PathBuf::from(path);
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|path| path.chars().any(char::is_control))
    {
        return Err(AddressError(
            "unix gateway address must be an absolute UTF-8 path",
        ));
    }
    Ok(path)
}

/// Loopback and the tailnet are always admitted. Any other unicast address
/// needs `allow_non_tailnet`. The unspecified addresses are refused either way.
pub(crate) fn admit_ip(ip: IpAddr, allow_non_tailnet: bool) -> Result<(), AddressError> {
    if ip.is_unspecified() {
        return Err(AddressError(
            "a gateway tcp address cannot be a wildcard bind",
        ));
    }
    if ip.is_loopback() || is_tailnet(ip) || allow_non_tailnet {
        return Ok(());
    }
    Err(AddressError(
        "tcp gateway address must be loopback or tailnet (100.64.0.0/10, fd7a:115c:a1e0::/48); set allow_non_tailnet to admit another address",
    ))
}

pub(crate) fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => u32::from(v4) & TAILNET_V4_MASK == TAILNET_V4,
        IpAddr::V6(v6) => u128::from(v6) >> TAILNET_V6_SHIFT == TAILNET_V6,
    }
}

/// The address is assigned on this machine. Binding a UDP socket to it is the
/// interface check: an address in range that this host does not own fails.
pub(crate) fn interface_has(ip: IpAddr) -> bool {
    std::net::UdpSocket::bind(SocketAddr::new(ip, 0)).is_ok()
}

pub(crate) fn absolute_file(path: &Path) -> Result<(), AddressError> {
    if path.is_absolute() && path.to_str().is_some() {
        Ok(())
    } else {
        Err(AddressError("gateway path must be an absolute UTF-8 path"))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn tailnet_and_loopback_are_admitted_and_wildcards_are_not() {
        assert!(is_tailnet(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_tailnet(IpAddr::V4(Ipv4Addr::new(100, 127, 255, 255))));
        assert!(!is_tailnet(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
        assert!(is_tailnet("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailnet("fd7a:115c:a1e1::1".parse().unwrap()));
        assert!(admit_ip(IpAddr::V4(Ipv4Addr::LOCALHOST), false).is_ok());
        assert!(admit_ip(IpAddr::V6(Ipv6Addr::LOCALHOST), false).is_ok());
        assert!(admit_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED), true).is_err());
        assert!(admit_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED), true).is_err());
        assert!(admit_ip(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)), false).is_err());
        assert!(admit_ip(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)), true).is_ok());
    }

    #[test]
    fn parses_unix_and_tcp_forms() {
        assert!(matches!(
            parse("unix:/tmp/plak/gateway.sock", false),
            Ok(GatewayAddress::Unix(_))
        ));
        assert!(parse("unix:relative.sock", false).is_err());
        assert!(parse("tcp:127.0.0.1:7787", false).is_ok());
        assert!(parse("tcp:100.100.1.2:7787", false).is_ok());
        assert!(parse("tcp:0.0.0.0:7787", true).is_err());
        assert!(parse("tcp:203.0.113.5:1", false).is_err());
    }

    #[test]
    fn loopback_is_a_local_interface() {
        assert!(interface_has(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!interface_has(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))));
    }
}
