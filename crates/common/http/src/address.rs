//! Destination address policy: which resolved addresses a client may connect to.
//!
//! The policy is checked on the exact socket address the transport then
//! connects, so a name that resolves differently between a caller's check and
//! the connection (DNS rebinding) cannot reach a refused address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A closed destination policy installed on a [`Client`](crate::Client).
///
/// Without a policy a client connects to whatever its origin resolves to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AddressPolicy(PolicyKind);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum PolicyKind {
    PublicOnly,
    /// Admits exactly one address, so connect-path tests can reach a loopback
    /// peer while every other resolved address is refused.
    #[cfg(test)]
    Only(IpAddr),
}

impl AddressPolicy {
    /// Admits only globally reachable unicast addresses: every loopback,
    /// private, link-local, shared, documentation, benchmarking, multicast,
    /// reserved and cloud-metadata range is refused, and IPv4 addresses
    /// embedded in IPv6 (mapped, NAT64, 6to4) are re-checked as IPv4. The
    /// host names `localhost`, `*.localhost` and `metadata.google.internal`
    /// are refused before resolution.
    #[must_use]
    pub const fn public_only() -> Self {
        Self(PolicyKind::PublicOnly)
    }

    #[cfg(test)]
    pub(crate) const fn only(address: IpAddr) -> Self {
        Self(PolicyKind::Only(address))
    }

    /// Whether the policy permits connecting to `address`.
    #[must_use]
    pub fn admits(self, address: IpAddr) -> bool {
        match self.0 {
            PolicyKind::PublicOnly => refused_range(address).is_none(),
            #[cfg(test)]
            PolicyKind::Only(only) => address == only,
        }
    }

    /// Whether the policy refuses this origin host name before resolution.
    /// Comparison ignores ASCII case and one trailing root dot.
    #[must_use]
    pub fn refuses_host_name(self, host: &str) -> bool {
        let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        host == "localhost" || host.ends_with(".localhost") || host == "metadata.google.internal"
    }

    pub(crate) fn identity_label(self) -> &'static [u8] {
        match self.0 {
            PolicyKind::PublicOnly => b"address-policy-public-only",
            #[cfg(test)]
            PolicyKind::Only(_) => b"address-policy-test-only",
        }
    }
}

/// What an IPv6 row means for an address inside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ipv6Rule {
    Refuse,
    /// The low 32 bits are an IPv4 address checked against the IPv4 table.
    EmbeddedLow32,
    /// 6to4: bits 16..48 are an IPv4 address checked against the IPv4 table.
    Embedded6to4,
    /// Local-use NAT64: only the /96 form is decoded; any other layout's
    /// translation is unknown and refused.
    LocalNat64,
}

struct Ipv4Row {
    network: Ipv4Addr,
    prefix: u8,
    reference: &'static str,
}

struct Ipv6Row {
    network: Ipv6Addr,
    prefix: u8,
    rule: Ipv6Rule,
    reference: &'static str,
}

const fn v4(network: [u8; 4], prefix: u8, reference: &'static str) -> Ipv4Row {
    Ipv4Row {
        network: Ipv4Addr::new(network[0], network[1], network[2], network[3]),
        prefix,
        reference,
    }
}

const fn v6(network: u128, prefix: u8, rule: Ipv6Rule, reference: &'static str) -> Ipv6Row {
    Ipv6Row {
        network: Ipv6Addr::from_bits(network),
        prefix,
        rule,
        reference,
    }
}

/// Refused IPv4 ranges (IANA IPv4 Special-Purpose Address Registry). The
/// first matching row names the refusal.
const IPV4_REFUSED: &[Ipv4Row] = &[
    v4([0, 0, 0, 0], 8, "RFC 791 this network"),
    v4([10, 0, 0, 0], 8, "RFC 1918 private"),
    v4([100, 64, 0, 0], 10, "RFC 6598 shared address space"),
    v4([127, 0, 0, 0], 8, "RFC 1122 loopback"),
    v4(
        [169, 254, 0, 0],
        16,
        "RFC 3927 link-local (cloud metadata 169.254.169.254)",
    ),
    v4([172, 16, 0, 0], 12, "RFC 1918 private"),
    v4([192, 0, 0, 0], 24, "RFC 6890 IETF protocol assignments"),
    v4([192, 0, 2, 0], 24, "RFC 5737 documentation TEST-NET-1"),
    v4(
        [192, 88, 99, 0],
        24,
        "RFC 7526 deprecated 6to4 relay anycast",
    ),
    v4([192, 168, 0, 0], 16, "RFC 1918 private"),
    v4([198, 18, 0, 0], 15, "RFC 2544 benchmarking"),
    v4([198, 51, 100, 0], 24, "RFC 5737 documentation TEST-NET-2"),
    v4([203, 0, 113, 0], 24, "RFC 5737 documentation TEST-NET-3"),
    v4([224, 0, 0, 0], 4, "RFC 5771 multicast"),
    v4([255, 255, 255, 255], 32, "RFC 919 limited broadcast"),
    v4([240, 0, 0, 0], 4, "RFC 1112 reserved"),
];

/// IPv6 ranges that are refused or carry an IPv4 address to re-check (IANA
/// IPv6 Special-Purpose Address Registry). The first matching row decides.
const IPV6_RULES: &[Ipv6Row] = &[
    v6(0, 128, Ipv6Rule::Refuse, "RFC 4291 unspecified"),
    v6(1, 128, Ipv6Rule::Refuse, "RFC 4291 loopback"),
    v6(
        0,
        96,
        Ipv6Rule::Refuse,
        "RFC 4291 deprecated IPv4-compatible",
    ),
    v6(
        0xffff_0000_0000,
        96,
        Ipv6Rule::EmbeddedLow32,
        "RFC 4291 IPv4-mapped",
    ),
    v6(
        0x0064_ff9b_0000_0000_0000_0000_0000_0000,
        96,
        Ipv6Rule::EmbeddedLow32,
        "RFC 6052 NAT64 well-known prefix",
    ),
    v6(
        0x0064_ff9b_0001_0000_0000_0000_0000_0000,
        48,
        Ipv6Rule::LocalNat64,
        "RFC 8215 local-use NAT64",
    ),
    v6(
        0x0100_0000_0000_0000_0000_0000_0000_0000,
        64,
        Ipv6Rule::Refuse,
        "RFC 6666 discard-only",
    ),
    v6(
        0x2001_0000_0000_0000_0000_0000_0000_0000,
        23,
        Ipv6Rule::Refuse,
        "RFC 2928 IETF protocol assignments",
    ),
    v6(
        0x2001_0db8_0000_0000_0000_0000_0000_0000,
        32,
        Ipv6Rule::Refuse,
        "RFC 3849 documentation",
    ),
    v6(
        0x2002_0000_0000_0000_0000_0000_0000_0000,
        16,
        Ipv6Rule::Embedded6to4,
        "RFC 3056 6to4",
    ),
    v6(
        0x3fff_0000_0000_0000_0000_0000_0000_0000,
        20,
        Ipv6Rule::Refuse,
        "RFC 9637 documentation",
    ),
    v6(
        0xfc00_0000_0000_0000_0000_0000_0000_0000,
        7,
        Ipv6Rule::Refuse,
        "RFC 4193 unique local (cloud metadata fd00:ec2::254)",
    ),
    v6(
        0xfe80_0000_0000_0000_0000_0000_0000_0000,
        10,
        Ipv6Rule::Refuse,
        "RFC 4291 link-local",
    ),
    v6(
        0xfec0_0000_0000_0000_0000_0000_0000_0000,
        10,
        Ipv6Rule::Refuse,
        "RFC 3879 deprecated site-local",
    ),
    v6(
        0xff00_0000_0000_0000_0000_0000_0000_0000,
        8,
        Ipv6Rule::Refuse,
        "RFC 4291 multicast",
    ),
];

/// The reference of the range that refuses `address`, or `None` when the
/// address is admitted as public.
fn refused_range(address: IpAddr) -> Option<&'static str> {
    match address {
        IpAddr::V4(address) => refused_v4(address),
        IpAddr::V6(address) => refused_v6(address),
    }
}

fn refused_v4(address: Ipv4Addr) -> Option<&'static str> {
    IPV4_REFUSED
        .iter()
        .find(|row| prefix_matches_v4(address, row.network, row.prefix))
        .map(|row| row.reference)
}

fn refused_v6(address: Ipv6Addr) -> Option<&'static str> {
    let row = IPV6_RULES
        .iter()
        .find(|row| prefix_matches_v6(address, row.network, row.prefix))?;
    let octets = address.octets();
    let low32 = Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]);
    match row.rule {
        Ipv6Rule::EmbeddedLow32 => refused_v4(low32),
        Ipv6Rule::Embedded6to4 => {
            refused_v4(Ipv4Addr::new(octets[2], octets[3], octets[4], octets[5]))
        }
        Ipv6Rule::LocalNat64 if octets[6..12].iter().all(|octet| *octet == 0) => refused_v4(low32),
        Ipv6Rule::Refuse | Ipv6Rule::LocalNat64 => Some(row.reference),
    }
}

fn prefix_matches_v4(address: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    address.to_bits() & mask == network.to_bits() & mask
}

fn prefix_matches_v6(address: Ipv6Addr, network: Ipv6Addr, prefix: u8) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
    address.to_bits() & mask == network.to_bits() & mask
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests parse literal addresses")]
mod tests {
    use super::*;

    fn refused(address: &str) -> Option<&'static str> {
        refused_range(address.parse::<IpAddr>().unwrap())
    }

    /// One case per table row: (row reference, addresses inside it, nearest
    /// addresses just outside it that are public).
    const IPV4_CASES: &[(&str, &[&str], &[&str])] = &[
        (
            "RFC 791 this network",
            &["0.0.0.0", "0.255.255.255"],
            &["1.0.0.0"],
        ),
        (
            "RFC 1918 private",
            &["10.0.0.0", "10.255.255.255"],
            &["9.255.255.255", "11.0.0.0"],
        ),
        (
            "RFC 6598 shared address space",
            &["100.64.0.0", "100.127.255.255"],
            &["100.63.255.255", "100.128.0.0"],
        ),
        (
            "RFC 1122 loopback",
            &["127.0.0.1", "127.255.255.255"],
            &["126.255.255.255", "128.0.0.0"],
        ),
        (
            "RFC 3927 link-local (cloud metadata 169.254.169.254)",
            &["169.254.0.0", "169.254.169.254"],
            &["169.253.255.255", "169.255.0.0"],
        ),
        (
            "RFC 1918 private",
            &["172.16.0.0", "172.31.255.255"],
            &["172.15.255.255", "172.32.0.0"],
        ),
        (
            "RFC 6890 IETF protocol assignments",
            &["192.0.0.0", "192.0.0.255"],
            &["192.0.1.0"],
        ),
        (
            "RFC 5737 documentation TEST-NET-1",
            &["192.0.2.0", "192.0.2.255"],
            &["192.0.3.0"],
        ),
        (
            "RFC 7526 deprecated 6to4 relay anycast",
            &["192.88.99.1"],
            &["192.88.98.255", "192.88.100.0"],
        ),
        (
            "RFC 1918 private",
            &["192.168.0.0", "192.168.255.255"],
            &["192.167.255.255", "192.169.0.0"],
        ),
        (
            "RFC 2544 benchmarking",
            &["198.18.0.0", "198.19.255.255"],
            &["198.17.255.255", "198.20.0.0"],
        ),
        (
            "RFC 5737 documentation TEST-NET-2",
            &["198.51.100.7"],
            &["198.51.99.255", "198.51.101.0"],
        ),
        (
            "RFC 5737 documentation TEST-NET-3",
            &["203.0.113.9"],
            &["203.0.112.255", "203.0.114.0"],
        ),
        (
            "RFC 5771 multicast",
            &["224.0.0.1", "239.255.255.255"],
            &["223.255.255.255"],
        ),
        ("RFC 919 limited broadcast", &["255.255.255.255"], &[]),
        ("RFC 1112 reserved", &["240.0.0.0", "255.255.255.254"], &[]),
    ];

    const IPV6_CASES: &[(&str, &[&str], &[&str])] = &[
        ("RFC 4291 unspecified", &["::"], &[]),
        ("RFC 4291 loopback", &["::1"], &[]),
        (
            "RFC 4291 deprecated IPv4-compatible",
            &["::2", "::8.8.8.8", "::ffff:ffff"],
            &["::1:0:0:0"],
        ),
        (
            "RFC 4291 IPv4-mapped",
            &[
                "::ffff:127.0.0.1",
                "::ffff:10.1.2.3",
                "::ffff:169.254.169.254",
            ],
            &["::ffff:8.8.8.8"],
        ),
        (
            "RFC 6052 NAT64 well-known prefix",
            &["64:ff9b::127.0.0.1", "64:ff9b::192.168.1.1"],
            &["64:ff9b::8.8.8.8"],
        ),
        (
            "RFC 8215 local-use NAT64",
            &[
                "64:ff9b:1::10.0.0.1",
                "64:ff9b:1:1::8.8.8.8",
                "64:ff9b:1:ffff:ffff:ffff:ffff:ffff",
            ],
            &["64:ff9b:1::8.8.8.8"],
        ),
        (
            "RFC 6666 discard-only",
            &["100::", "100::ffff:ffff:ffff:ffff"],
            &["100:0:0:1::"],
        ),
        (
            "RFC 2928 IETF protocol assignments",
            &["2001::1", "2001:1ff:ffff::"],
            &["2001:200::1"],
        ),
        (
            "RFC 3849 documentation",
            &["2001:db8::1", "2001:db8:ffff::"],
            &["2001:db9::1"],
        ),
        (
            "RFC 3056 6to4",
            &["2002:7f00:1::", "2002:a00:1::1", "2002:a9fe:a9fe::"],
            &["2002:808:808::1"],
        ),
        (
            "RFC 9637 documentation",
            &["3fff::1", "3fff:fff::"],
            &["3fff:1000::1"],
        ),
        (
            "RFC 4193 unique local (cloud metadata fd00:ec2::254)",
            &["fc00::1", "fd00:ec2::254", "fdff::"],
            &["fbff:ffff::1"],
        ),
        ("RFC 4291 link-local", &["fe80::1", "febf:ffff::"], &[]),
        (
            "RFC 3879 deprecated site-local",
            &["fec0::1", "feff:ffff::"],
            &[],
        ),
        ("RFC 4291 multicast", &["ff00::", "ff02::1"], &[]),
    ];

    #[test]
    fn every_ipv4_row_refuses_its_range_and_admits_its_neighbors() {
        for (reference, inside, outside) in IPV4_CASES {
            for address in *inside {
                assert_eq!(refused(address), Some(*reference), "{address}");
            }
            for address in *outside {
                assert_eq!(refused(address), None, "{address}");
            }
        }
    }

    #[test]
    fn every_ipv6_row_refuses_or_rechecks_its_range() {
        for (reference, inside, outside) in IPV6_CASES {
            for address in *inside {
                let parsed = address.parse::<Ipv6Addr>().unwrap();
                let row = IPV6_RULES
                    .iter()
                    .find(|row| prefix_matches_v6(parsed, row.network, row.prefix))
                    .map(|row| row.reference);
                assert_eq!(row, Some(*reference), "{address}");
                assert!(refused(address).is_some(), "{address}");
            }
            for address in *outside {
                assert_eq!(refused(address), None, "{address}");
            }
        }
    }

    #[test]
    fn every_table_row_has_a_case() {
        for row in IPV4_REFUSED {
            assert!(
                IPV4_CASES.iter().any(|case| case.0 == row.reference),
                "{}",
                row.reference
            );
        }
        for row in IPV6_RULES {
            assert!(
                IPV6_CASES.iter().any(|case| case.0 == row.reference),
                "{}",
                row.reference
            );
        }
    }

    #[test]
    fn embedded_ipv4_is_rechecked_with_the_ipv4_reference() {
        assert_eq!(refused("::ffff:127.0.0.1"), Some("RFC 1122 loopback"));
        assert_eq!(
            refused("64:ff9b::a9fe:a9fe"),
            Some("RFC 3927 link-local (cloud metadata 169.254.169.254)")
        );
        assert_eq!(refused("64:ff9b:1::10.0.0.1"), Some("RFC 1918 private"));
        assert_eq!(refused("2002:7f00:1::"), Some("RFC 1122 loopback"));
        assert_eq!(
            refused("64:ff9b:1:1::8.8.8.8"),
            Some("RFC 8215 local-use NAT64")
        );
    }

    #[test]
    fn public_addresses_are_admitted() {
        let policy = AddressPolicy::public_only();
        for address in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
        ] {
            assert!(policy.admits(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn host_name_literals_are_refused_before_resolution() {
        let policy = AddressPolicy::public_only();
        for host in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "a.localhost",
            "x.y.Localhost.",
            "metadata.google.internal",
            "Metadata.Google.Internal.",
        ] {
            assert!(policy.refuses_host_name(host), "{host}");
        }
        for host in [
            "example.com",
            "localhost.example.com",
            "notlocalhost",
            "google.internal",
            "metadata.google.internal.example",
        ] {
            assert!(!policy.refuses_host_name(host), "{host}");
        }
    }
}
