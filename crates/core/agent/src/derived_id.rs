//! The byte-preserving `UUIDv8` projection shared by deterministic agent identities.

use plakous_model::Digest;

pub(super) fn bytes(digest: Digest) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_model::DigestDomain;

    #[test]
    fn derived_uuid_bytes_keep_the_existing_identity_pins() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut sequential = [0; 32];
        for (byte, value) in sequential.iter_mut().zip(0_u8..32) {
            *byte = value;
        }
        for (input, expected) in [
            ([0; 32], "00000000-0000-8000-8000-000000000000"),
            ([255; 32], "ffffffff-ffff-8fff-bfff-ffffffffffff"),
            (sequential, "00010203-0405-8607-8809-0a0b0c0d0e0f"),
        ] {
            let digest = Digest::from_sha256_bytes(DigestDomain::ProviderOperation, 1, input)?;
            assert_eq!(uuid::Uuid::from_bytes(bytes(digest)).to_string(), expected);
        }
        Ok(())
    }
}
