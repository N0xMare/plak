//! Random gateway ids, shared by the relay client and the relay server core.

use plakous_gateway_protocol::UuidText;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// A fresh random version 4 UUID.
pub(crate) fn random_uuid() -> Option<UuidText> {
    let mut bytes = [0_u8; 16];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut hex = String::with_capacity(32);
    for byte in bytes {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    let text = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    UuidText::new(text).ok()
}
