use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use plakous_runner_protocol::{VersionTriple, frame::MAX_FRAME_BYTES};
use proptest::prelude::*;
use secrecy::ExposeSecret;

const URL: &str = "wss://example.test/runner";
fn start() -> (Handshake, Handshake) {
    let runner = StaticKey::from_bytes([1; 32]);
    let host = StaticKey::from_bytes([2; 32]);
    let version = VersionTriple::current("source", "release");
    (
        Handshake::initiator(&runner, &host.public_key(), &version, URL, "host").unwrap(),
        Handshake::responder(&host, &version, URL, "host").unwrap(),
    )
}
fn connected() -> (Transport, Transport) {
    let (runner, host) = start();
    let (runner, m1) = runner.write().unwrap();
    assert_eq!(m1.len(), 96);
    let host = host.read(&m1).unwrap();
    let (host, m2) = host.write().unwrap();
    assert_eq!(m2.len(), 48);
    let runner = runner.read(&m2).unwrap();
    (runner.finish().unwrap(), host.finish().unwrap())
}

#[test]
fn ik_round_trip_and_replay_poisoning() {
    let (mut runner, mut host) = connected();
    assert_eq!(runner.handshake_hash(), host.handshake_hash());
    assert_eq!(
        *host.remote_public_key(),
        StaticKey::from_bytes([1; 32]).public_key()
    );
    let ciphertext = runner.encrypt(b"hello").unwrap();
    assert_eq!(host.decrypt(&ciphertext).unwrap(), b"hello");
    let answer = host.encrypt(b"answer").unwrap();
    assert_eq!(runner.decrypt(&answer).unwrap(), b"answer");
    assert!(host.decrypt(&ciphertext).is_err());
    assert!(host.encrypt(b"poisoned").is_err());
}

#[test]
fn transport_boundaries_and_reordering() {
    let (mut runner, mut host) = connected();
    let body = vec![0x77; MAX_PLAINTEXT_BYTES];
    let packet = runner.encrypt(&body).unwrap();
    assert_eq!(packet.len(), 65535);
    assert_eq!(host.decrypt(&packet).unwrap(), body);
    assert!(runner.encrypt(&vec![0; MAX_PLAINTEXT_BYTES + 1]).is_err());
    assert!(runner.encrypt(b"poisoned").is_err());
    let (mut runner, mut host) = connected();
    let first = runner.encrypt(b"one").unwrap();
    let second = runner.encrypt(b"two").unwrap();
    assert!(host.decrypt(&second).is_err());
    assert!(host.decrypt(&first).is_err());
    for length in [0, 15, 65536] {
        let (_, mut host) = connected();
        assert!(matches!(
            host.decrypt(&vec![0; length]),
            Err(Error::Invalid)
        ));
    }
}

#[test]
fn wrong_pin_and_tampered_messages_fail() {
    let (runner, _) = start();
    let (_, message) = runner.write().unwrap();
    let wrong_host = StaticKey::from_bytes([3; 32]);
    assert!(
        Handshake::responder(
            &wrong_host,
            &VersionTriple::current("source", "release"),
            URL,
            "host"
        )
        .unwrap()
        .read(&message)
        .is_err()
    );
    for index in [0, 32, 80] {
        let (runner, host) = start();
        let (_, mut message) = runner.write().unwrap();
        message[index] ^= 1;
        assert!(host.read(&message).is_err());
    }
    for index in [0, 32, 47] {
        let (runner, host) = start();
        let (runner, m1) = runner.write().unwrap();
        let (_, mut m2) = host.read(&m1).unwrap().write().unwrap();
        m2[index] ^= 1;
        assert!(runner.read(&m2).is_err());
    }
}

#[test]
fn exact_handshake_sizes() {
    for length in [0, 47, 48, 95, 97, 112, 65536] {
        let (_, host) = start();
        assert!(matches!(host.read(&vec![0; length]), Err(Error::Invalid)));
    }
    for length in [0, 47, 49, 64, 96, 65536] {
        let (runner, _) = start();
        let (runner, _) = runner.write().unwrap();
        assert!(matches!(runner.read(&vec![0; length]), Err(Error::Invalid)));
    }
}

#[test]
fn prologue_binds_every_component_and_field_boundary() {
    let runner_key = StaticKey::from_bytes([1; 32]);
    let host_key = StaticKey::from_bytes([2; 32]);
    let version = VersionTriple::current("source", "release");
    for (version, url, host_id) in [
        (
            VersionTriple {
                protocol: 2,
                ..version.clone()
            },
            URL,
            "host",
        ),
        (VersionTriple::current("different", "release"), URL, "host"),
        (VersionTriple::current("source", "debug"), URL, "host"),
        (version.clone(), "wss://other.test/runner", "host"),
        (version.clone(), URL, "other"),
        (VersionTriple::current("sourc", "erelease"), URL, "host"),
    ] {
        let runner =
            Handshake::initiator(&runner_key, &host_key.public_key(), &version, url, host_id)
                .unwrap();
        let (_, m1) = runner.write().unwrap();
        let (_, host) = start();
        assert!(host.read(&m1).is_err());
    }
}

#[test]
fn replayed_message_one_cannot_confirm_runner() {
    let (runner, host) = start();
    let (runner, m1) = runner.write().unwrap();
    let (host, m2) = host.read(&m1).unwrap().write().unwrap();
    let mut runner = runner.read(&m2).unwrap().finish().unwrap();
    let mut host = host.finish().unwrap();
    let key = PairKey::derive(&[7; 16]);
    let proof = key.prove([8; 16], runner.handshake_hash());
    let packet = runner
        .encrypt(&serde_json::to_vec(&proof).unwrap())
        .unwrap();
    assert!(host.decrypt(&packet).is_ok());
    let (_, replay_host) = start();
    let (replay_host, _) = replay_host.read(&m1).unwrap().write().unwrap();
    let mut replay_host = replay_host.finish().unwrap();
    assert_ne!(host.handshake_hash(), replay_host.handshake_hash());
    assert!(replay_host.decrypt(&packet).is_err());
    assert!(!key.verify(&proof, replay_host.handshake_hash()));
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        write!(&mut out, "{b:02x}").unwrap();
        out
    })
}
#[test]
fn pair_proof_known_answer_and_session_binding() {
    let secret = std::array::from_fn(|index| u8::try_from(index).unwrap());
    let hash = std::array::from_fn(|index| u8::try_from(index).unwrap());
    assert_eq!(
        hex(&pairing::mac(&secret, b"plak-dial/1 pair")),
        "bc6facbfbc997c73d73289a6302b7d9ac86e19f793ea219a88adc407b3ff79cd"
    );
    let key = PairKey::derive(&secret);
    let mut proof = key.prove([9; 16], &hash);
    assert_eq!(
        hex(&proof.mac),
        "0026ac76c3918f592b296bb80af39aaa6a86427e88d9e0ffa8396bc33141811e"
    );
    assert!(key.verify(&proof, &hash));
    assert!(!key.verify(&proof, &[1; 32]));
    proof.mac[0] ^= 1;
    assert!(!key.verify(&proof, &hash));
}

fn offer() -> Offer {
    Offer::generate(URL.into(), "host".into(), [2; 32], "runner".into(), 123_456).unwrap()
}
fn replace_field(text: &str, index: usize, value: &[u8]) -> String {
    let mut fields: Vec<String> = text
        .strip_prefix("plak-dial:1:")
        .unwrap()
        .split('.')
        .map(str::to_owned)
        .collect();
    fields[index] = URL_SAFE_NO_PAD.encode(value);
    format!("plak-dial:1:{}", fields.join("."))
}
#[test]
fn offer_round_trip_and_rejections() {
    let original = offer();
    let encoded = original.expose_encoded();
    let text = encoded.expose_secret();
    let parsed = Offer::parse(text).unwrap();
    assert_eq!(parsed.expose_encoded().expose_secret(), text);
    assert_eq!(parsed.url(), URL);
    assert_eq!(parsed.host_id(), "host");
    assert_eq!(parsed.host_key(), &[2; 32]);
    assert_eq!(parsed.runner_name(), "runner");
    assert_eq!(parsed.expiry(), 123_456);
    let proof = parsed.pair_key().prove(parsed.offer_id(), &[3; 32]);
    assert!(original.pair_key().verify(&proof, &[3; 32]));
    for url in [
        "https://example.test",
        "ftp://example.test",
        "wss://user@example.test",
        "wss://@example.test",
        "wss://u:p@example.test",
        "wss://example.test/?q",
        "wss://example.test/?",
        "wss://example.test/#f",
        "wss://example.test/#",
        "wss://",
        "wss://example.test/\n",
        "wss:\\example.test",
        "wss://example.test/a\\b",
    ] {
        assert!(
            Offer::parse(&replace_field(text, 0, url.as_bytes())).is_err(),
            "{url}"
        );
    }
    for (index, bytes) in [
        (1, &b""[..]),
        (2, &b"short"[..]),
        (3, &b"short"[..]),
        (4, &b"short"[..]),
        (5, &b""[..]),
        (6, &b"-1"[..]),
        (6, &b"+1"[..]),
        (6, &b"18446744073709551616"[..]),
        (5, &b"\xff"[..]),
    ] {
        assert!(Offer::parse(&replace_field(text, index, bytes)).is_err());
    }
    for bad in [
        String::new(),
        text.replace("plak-dial:1:", "plak-dial:2:"),
        format!("{text}.extra"),
        text.rsplit_once('.').unwrap().0.to_owned(),
        text.replace('.', "=."),
        text.replace('.', "!."),
    ] {
        assert!(Offer::parse(&bad).is_err());
    }
}

#[test]
fn ws_is_compile_time_test_only() {
    let result = Offer::parse(&replace_field(
        offer().expose_encoded().expose_secret(),
        0,
        b"ws://localhost/runner",
    ));
    assert_eq!(result.is_ok(), cfg!(feature = "test-hooks"));
}

#[test]
fn fingerprints_and_confirmation() {
    let fingerprint = Fingerprint::of(&[0; 32]);
    assert_eq!(
        fingerprint.to_string(),
        "320b 5ea9 9e65 3bc2 b593 db41 30d1 0a4e"
    );
    let groups: Vec<_> = fingerprint
        .to_string()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    for _ in 0..64 {
        let challenge = Confirmation::choose().unwrap();
        assert!(
            challenge
                .positions()
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert!(challenge.positions().iter().all(|&index| index < 8));
        let typed = challenge.positions().map(|i| groups[i].clone()).join(" ");
        assert!(challenge.check(fingerprint, &typed.to_uppercase()));
        for bad in [
            "y",
            "",
            "320b",
            "0000 0000 0000 0000",
            &format!("{typed} 0000"),
        ] {
            assert!(!challenge.check(fingerprint, bad));
        }
    }
}

#[test]
fn debug_never_exposes_material() {
    let (handshake, _) = start();
    let (transport, _) = connected();
    let key = PairKey::derive(&[42; 16]);
    let proof = key.prove([43; 16], &[44; 32]);
    for debug in [
        format!("{:?}", StaticKey::from_bytes([41; 32])),
        format!("{handshake:?}"),
        format!("{transport:?}"),
        format!("{key:?}"),
        format!("{proof:?}"),
        format!("{:?}", offer()),
        format!("{:?}", offer().expose_encoded()),
        format!("{:?}", FrameAssembler::new()),
    ] {
        assert!(debug.ends_with("([REDACTED])"), "{debug}");
    }
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut frame = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    frame
}
#[test]
fn framing_boundaries_cap_and_truncation() {
    for size in [
        0,
        MAX_PLAINTEXT_BYTES - 4,
        MAX_PLAINTEXT_BYTES - 3,
        MAX_PLAINTEXT_BYTES,
        MAX_FRAME_BYTES,
    ] {
        let body = vec![b' '; size];
        let encoded = frame(&body);
        let chunks: Vec<_> = split_frame(&encoded).unwrap().collect();
        assert_eq!(chunks.len(), encoded.len().div_ceil(MAX_PLAINTEXT_BYTES));
        let mut assembler = FrameAssembler::new();
        let mut output = Vec::new();
        for chunk in chunks {
            output.extend(assembler.push(chunk).unwrap());
        }
        assert_eq!(output, vec![body]);
        assembler.finish().unwrap();
    }
    assert!(split_frame(&frame(&vec![0; MAX_FRAME_BYTES + 1])).is_err());
    assert!(split_frame(&[0, 0, 0, 2, 1]).is_err());
    assert!(split_frame(&[0, 0]).is_err());
    let mut assembler = FrameAssembler::new();
    assert!(assembler.push(&vec![0; MAX_PLAINTEXT_BYTES + 1]).is_err());
    assert!(assembler.push(&[]).is_err());
    let mut assembler = FrameAssembler::new();
    let prefix = u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_be_bytes();
    assembler.push(&prefix[..3]).unwrap();
    assert!(assembler.push(&prefix[3..]).is_err());
    assert!(assembler.finish().is_err());
    let mut assembler = FrameAssembler::new();
    assembler.push(&[0, 0, 0, 1]).unwrap();
    assert!(assembler.finish().is_err());
    let mut coalesced = frame(b"{}");
    coalesced.extend(frame(b"[]"));
    let mut assembler = FrameAssembler::new();
    assert_eq!(
        assembler.push(&coalesced).unwrap(),
        vec![b"{}".to_vec(), b"[]".to_vec()]
    );
    assembler.finish().unwrap();
}

proptest! {
    #[test]
    fn generated_offers_round_trip(host in "[a-z0-9]{1,64}", runner in "[a-z0-9]{1,64}", path in "[a-z0-9/]{0,64}", expiry in any::<u64>(), key in any::<[u8;32]>()) {
        let url = format!("wss://example.test/{path}");
        let offer = Offer::generate(url.clone(), host.clone(), key, runner.clone(), expiry).unwrap();
        let encoded = offer.expose_encoded();
        let parsed = Offer::parse(encoded.expose_secret()).unwrap();
        prop_assert_eq!(parsed.url(), url);
        prop_assert_eq!(parsed.host_id(), host);
        prop_assert_eq!(parsed.runner_name(), runner);
        prop_assert_eq!(parsed.expiry(), expiry);
        prop_assert_eq!(parsed.host_key(), &key);
    }
    #[test]
    fn arbitrary_offer_fields_never_panic(fields in prop::collection::vec(prop::collection::vec(any::<u8>(),0..256),0..10)) {
        let text = format!("plak-dial:1:{}", fields.iter().map(|f| URL_SAFE_NO_PAD.encode(f)).collect::<Vec<_>>().join("."));
        let _ = Offer::parse(&text);
    }
    #[test]
    fn arbitrary_offer_parser_never_panics(text in ".{0,4096}") {
        if let Ok(offer) = Offer::parse(&text) {
            let encoded = offer.expose_encoded();
            prop_assert!(Offer::parse(encoded.expose_secret()).is_ok());
        }
    }
    #[test]
    fn arbitrary_frame_splits_reassemble(body in prop::collection::vec(any::<u8>(),0..200_000), chunk in 1usize..=MAX_PLAINTEXT_BYTES) {
        let encoded = frame(&body);
        let mut assembler = FrameAssembler::new();
        let mut output = Vec::new();
        for piece in encoded.chunks(chunk) { output.extend(assembler.push(piece).unwrap()); }
        prop_assert_eq!(output, vec![body]);
        prop_assert!(assembler.finish().is_ok());
    }
    #[test]
    fn arbitrary_reassembly_input_is_bounded(bytes in prop::collection::vec(any::<u8>(),0..100_000), chunk in 1usize..70_000) {
        let mut assembler = FrameAssembler::new();
        for piece in bytes.chunks(chunk) {
            if assembler.push(piece).is_err() { prop_assert!(assembler.push(&[]).is_err()); return Ok(()); }
        }
        let _ = assembler.finish();
    }
}

fn unhex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect()
}
#[test]
fn official_cacophony_vector() {
    // Nonempty vector payloads exercise the resolver directly; the public dial
    // wrapper deliberately accepts only the empty-payload dial protocol.
    let data: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/cacophony-ik.json")).unwrap();
    let v = &data["vectors"][0];
    let bytes = |field: &str| unhex(v[field].as_str().unwrap());
    let init_static = bytes("init_static");
    let init_ephemeral = bytes("init_ephemeral");
    let remote = bytes("init_remote_static");
    let resp_static = bytes("resp_static");
    let resp_ephemeral = bytes("resp_ephemeral");
    let prologue = bytes("init_prologue");
    let make = || {
        snow::Builder::with_resolver(
            session::SUITE.parse().unwrap(),
            Box::new(resolver::Resolver),
        )
    };
    let mut init = make()
        .local_private_key(&init_static)
        .unwrap()
        .remote_public_key(&remote)
        .unwrap()
        .prologue(&prologue)
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&init_ephemeral)
        .build_initiator()
        .unwrap();
    let mut resp = make()
        .local_private_key(&resp_static)
        .unwrap()
        .prologue(&prologue)
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&resp_ephemeral)
        .build_responder()
        .unwrap();
    let mut out = vec![0; 65535];
    let mut plain = vec![0; 65535];
    let messages = v["messages"].as_array().unwrap();
    for (index, message) in messages[..2].iter().enumerate() {
        let payload = unhex(message["payload"].as_str().unwrap());
        let expected = unhex(message["ciphertext"].as_str().unwrap());
        let (sender, receiver) = if index == 0 {
            (&mut init, &mut resp)
        } else {
            (&mut resp, &mut init)
        };
        let len = sender.write_message(&payload, &mut out).unwrap();
        assert_eq!(&out[..len], expected);
        let len = receiver.read_message(&expected, &mut plain).unwrap();
        assert_eq!(&plain[..len], payload);
    }
    assert_eq!(init.get_handshake_hash(), bytes("handshake_hash"));
    assert_eq!(resp.get_handshake_hash(), bytes("handshake_hash"));
    let mut init = init.into_transport_mode().unwrap();
    let mut resp = resp.into_transport_mode().unwrap();
    for (index, message) in messages[2..].iter().enumerate() {
        let payload = unhex(message["payload"].as_str().unwrap());
        let expected = unhex(message["ciphertext"].as_str().unwrap());
        let (sender, receiver) = if index % 2 == 0 {
            (&mut init, &mut resp)
        } else {
            (&mut resp, &mut init)
        };
        let len = sender.write_message(&payload, &mut out).unwrap();
        assert_eq!(&out[..len], expected);
        let len = receiver.read_message(&expected, &mut plain).unwrap();
        assert_eq!(&plain[..len], payload);
    }
}

#[test]
fn resolver_refuses_all_zero_dh_and_other_suites() {
    use snow::{
        params::{CipherChoice, DHChoice, HashChoice},
        resolvers::CryptoResolver,
    };
    let resolver = resolver::Resolver;
    let mut dh = resolver.resolve_dh(&DHChoice::Curve25519).unwrap();
    dh.set(&[3; 32]);
    for public in [[0; 32], {
        let mut p = [0; 32];
        p[0] = 1;
        p
    }] {
        let mut output = [0x55; 32];
        assert!(dh.dh(&public, &mut output).is_err());
        assert_eq!(output, [0x55; 32]);
    }
    assert!(resolver.resolve_hash(&HashChoice::SHA256).is_none());
    assert!(resolver.resolve_cipher(&CipherChoice::AESGCM).is_none());
}

#[test]
fn entropy_failure_has_no_fallback() {
    use snow::resolvers::CryptoResolver;
    struct Failed;
    impl snow::types::Random for Failed {
        fn try_fill_bytes(&mut self, _: &mut [u8]) -> Result<(), snow::Error> {
            Err(snow::Error::Rng)
        }
    }
    let mut dh = resolver::Resolver
        .resolve_dh(&snow::params::DHChoice::Curve25519)
        .unwrap();
    assert!(matches!(dh.generate(&mut Failed), Err(snow::Error::Rng)));
}
