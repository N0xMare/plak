use crate::Error;
use blake2::{
    Blake2s256, Blake2sMac256, Digest,
    digest::{KeyInit, Mac},
};
use secrecy::{ExposeSecret, SecretBox};
use std::fmt;

pub(crate) fn mac(key: &[u8], input: &[u8]) -> [u8; 32] {
    let mut state = <Blake2sMac256 as KeyInit>::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("pair keys are at most 32 bytes"));
    Mac::update(&mut state, input);
    state.finalize().into_bytes().into()
}

/// A zeroizing pairing MAC key, not a password hash or an authorization record.
pub struct PairKey(SecretBox<[u8; 32]>);
impl fmt::Debug for PairKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairKey([REDACTED])")
    }
}
impl PairKey {
    /// Derives the stored MAC key from a 128-bit offer secret.
    #[must_use]
    pub fn derive(secret: &[u8; 16]) -> Self {
        Self(SecretBox::new(Box::new(mac(secret, b"plak-dial/1 pair"))))
    }
    /// Makes a proof for this session; send it only as encrypted transport data.
    #[must_use]
    pub fn prove(&self, offer_id: [u8; 16], handshake_hash: &[u8; 32]) -> PairProof {
        PairProof {
            offer_id,
            mac: mac(self.0.expose_secret(), handshake_hash),
        }
    }
    /// Checks the MAC in constant time. The caller owns offer lookup and consumption.
    #[must_use]
    pub fn verify(&self, proof: &PairProof, handshake_hash: &[u8; 32]) -> bool {
        use subtle::ConstantTimeEq;
        bool::from(mac(self.0.expose_secret(), handshake_hash).ct_eq(&proof.mac))
    }
}

/// Pairing key confirmation, bound to exactly one completed IK session.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairProof {
    /// Non-secret identifier; select the live offer before verifying its MAC.
    pub offer_id: [u8; 16],
    /// BLAKE2s keyed MAC over the handshake hash.
    pub mac: [u8; 32],
}
impl fmt::Debug for PairProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairProof([REDACTED])")
    }
}

/// A 128-bit BLAKE2s public-key fingerprint, safe to display.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fingerprint([u8; 16]);
impl Fingerprint {
    /// Hashes a public key and truncates to its first 128 bits.
    #[must_use]
    pub fn of(public_key: &[u8; 32]) -> Self {
        let digest = Blake2s256::digest(public_key);
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        Self(bytes)
    }
    fn group(self, index: usize) -> String {
        format!("{:02x}{:02x}", self.0[index * 2], self.0[index * 2 + 1])
    }
}
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for index in 0..8 {
            if index > 0 {
                f.write_str(" ")?;
            }
            f.write_str(&self.group(index))?;
        }
        Ok(())
    }
}

/// Four distinct unpredictable fingerprint group positions, chosen at confirmation time.
#[derive(Debug)]
pub struct Confirmation {
    positions: [usize; 4],
}
impl Confirmation {
    /// Selects four groups uniformly without replacement, then sorts for presentation.
    /// # Errors
    /// Refuses an unavailable platform CSPRNG.
    pub fn choose() -> Result<Self, Error> {
        let mut groups = [0, 1, 2, 3, 4, 5, 6, 7];
        for index in 0..4 {
            let count = u8::try_from(8 - index).map_err(|_| Error::Invalid)?;
            let limit = 256_u16 - 256_u16 % u16::from(count);
            let selected = loop {
                let mut byte = [0];
                getrandom::fill(&mut byte).map_err(Error::Entropy)?;
                if u16::from(byte[0]) < limit {
                    break usize::from(byte[0] % count);
                }
            };
            groups.swap(index, index + selected);
        }
        let mut positions = [groups[0], groups[1], groups[2], groups[3]];
        positions.sort_unstable();
        Ok(Self { positions })
    }
    /// Zero-based group positions, ascending. Display them to the operator as one-based.
    #[must_use]
    pub fn positions(&self) -> &[usize; 4] {
        &self.positions
    }
    /// Requires exactly four whitespace-separated hexadecimal groups in requested order.
    #[must_use]
    pub fn check(&self, fingerprint: Fingerprint, typed: &str) -> bool {
        let groups: Vec<_> = typed.split_whitespace().collect();
        groups.len() == 4
            && self
                .positions
                .iter()
                .zip(groups)
                .all(|(&position, typed)| typed.eq_ignore_ascii_case(&fingerprint.group(position)))
    }
}
