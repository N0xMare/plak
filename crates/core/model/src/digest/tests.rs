use super::*;

#[test]
fn streamed_single_part_is_exact_and_length_bound() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = vec![0x5a; 65_537];
    let expected = Digest::sha256(DigestDomain::Blob, 7, &[&bytes])?;
    let mut complete = io::Cursor::new(&bytes);
    assert_eq!(
        Digest::sha256_reader(
            DigestDomain::Blob,
            7,
            u64::try_from(bytes.len())?,
            &mut complete,
        )?,
        expected,
    );
    let mut short = io::Cursor::new(&bytes[..bytes.len() - 1]);
    assert!(matches!(
        Digest::sha256_reader(
            DigestDomain::Blob,
            7,
            u64::try_from(bytes.len())?,
            &mut short,
        ),
        Err(DigestStreamError::LengthMismatch)
    ));
    Ok(())
}

#[test]
fn domain_and_version_are_part_of_identity() -> Result<(), DigestError> {
    let replay = Digest::sha256(DigestDomain::ReplayEnvelope, 1, &[b"same"])?;
    let blob = Digest::sha256(DigestDomain::Blob, 1, &[b"same"])?;
    let authorization = Digest::sha256(DigestDomain::EffectAuthorization, 1, &[b"same"])?;
    let environment = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"same"])?;
    let upgraded = Digest::sha256(DigestDomain::ReplayEnvelope, 2, &[b"same"])?;
    assert_ne!(replay, blob);
    assert_ne!(authorization, environment);
    assert_ne!(replay, upgraded);
    assert_eq!(replay.domain(), DigestDomain::ReplayEnvelope);
    Ok(())
}

#[test]
fn framing_distinguishes_part_boundaries() -> Result<(), DigestError> {
    let split = Digest::sha256(DigestDomain::Blob, 1, &[b"a", b"bc"])?;
    let joined = Digest::sha256(DigestDomain::Blob, 1, &[b"ab", b"c"])?;
    assert_ne!(split, joined);
    Ok(())
}

#[test]
fn complete_identity_encoding_includes_domain_and_version() -> Result<(), Box<dyn std::error::Error>>
{
    let original = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"producer"])?;
    let mut foreign_domain = serde_json::to_value(original)?;
    foreign_domain["domain"] = serde_json::json!("Blob");
    let foreign_domain = serde_json::from_value::<Digest>(foreign_domain)?;
    let mut upgraded = serde_json::to_value(original)?;
    upgraded["version"] = serde_json::json!(2);
    let upgraded = serde_json::from_value::<Digest>(upgraded)?;

    assert_eq!(original.bytes(), foreign_domain.bytes());
    assert_eq!(original.bytes(), upgraded.bytes());
    assert_ne!(original.identity_bytes(), foreign_domain.identity_bytes());
    assert_ne!(original.identity_bytes(), upgraded.identity_bytes());
    Ok(())
}

#[test]
fn digest_framing_has_a_frozen_vector() -> Result<(), DigestError> {
    let digest = Digest::sha256(
        DigestDomain::ReplayEnvelope,
        1,
        &[b"protocol", b"family", b"{\"opaque\":true}"],
    )?;
    assert_eq!(
        digest.to_string(),
        "0eb6819a6ea55fa9dc8f1dbc7d51bae38f68cef4e929df3b1651531fe28127b8"
    );
    Ok(())
}

#[test]
fn unframed_sha256_matches_a_frozen_vector_and_concatenates() {
    assert_eq!(
        sha256_hex(&[b"abc"]),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(sha256_hex(&[b"ab", b"c"]), sha256_hex(&[b"a", b"bc"]));
    assert_eq!(hex_lower(&sha256_bytes(&[b"abc"])), sha256_hex(&[b"abc"]));
}

#[test]
fn framed_digest_does_not_collide_where_the_unframed_primitive_does() -> Result<(), DigestError> {
    assert_ne!(
        Digest::sha256(DigestDomain::Blob, 1, &[b"ab", b"c"])?,
        Digest::sha256(DigestDomain::Blob, 1, &[b"a", b"bc"])?,
    );
    Ok(())
}

#[test]
fn hex_lower_encodes_every_nibble() {
    assert_eq!(hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    assert_eq!(hex_lower(&[]), "");
}

#[test]
fn deserialization_rejects_reserved_version_zero() -> Result<(), serde_json::Error> {
    let digest = Digest::sha256(DigestDomain::Blob, 1, &[b"value"])
        .map_err(|error| serde_json::Error::io(io::Error::other(error)))?;
    let mut value = serde_json::to_value(digest)?;
    value["version"] = serde_json::json!(0);
    assert!(serde_json::from_value::<Digest>(value).is_err());
    Ok(())
}
