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
