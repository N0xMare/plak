use crate::{Error, resolver::Resolver};
use plakous_runner_protocol::VersionTriple;
use secrecy::{ExposeSecret, SecretBox};
use std::fmt;

pub(crate) const SUITE: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// An owned, non-cloneable X25519 static private key, zeroized on drop.
pub struct StaticKey(SecretBox<[u8; 32]>);
impl StaticKey {
    /// Takes ownership of private key bytes. The caller must erase any other copies.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(SecretBox::new(Box::new(bytes)))
    }
    /// Generates a key using the platform CSPRNG.
    /// # Errors
    /// Returns an entropy failure, never a weak fallback.
    pub fn generate() -> Result<Self, Error> {
        let mut bytes = zeroize::Zeroizing::new([0; 32]);
        getrandom::fill(&mut *bytes).map_err(Error::Entropy)?;
        Ok(Self::from_bytes(*bytes))
    }
    /// The corresponding public key; show a fingerprint, not these bytes, in diagnostics.
    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*self.0.expose_secret()))
            .to_bytes()
    }
}
impl fmt::Debug for StaticKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticKey([REDACTED])")
    }
}

/// One IK handshake. Consuming operations prevent reuse after any failure.
pub struct Handshake(snow::HandshakeState);
impl fmt::Debug for Handshake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Handshake([REDACTED])")
    }
}

fn prologue(version: &VersionTriple, url: &str, host_id: &str) -> Result<Vec<u8>, Error> {
    super::offer::validate_url(url)?;
    let mut bytes = b"plak-dial/1".to_vec();
    bytes.extend_from_slice(&version.protocol.to_be_bytes());
    // Length prefixes make the variable-width concatenation unambiguous.
    for field in [&version.source_revision, &version.flavor, url, host_id] {
        let length = u32::try_from(field.len()).map_err(|_| Error::Invalid)?;
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    Ok(bytes)
}

impl Handshake {
    /// Starts the runner role with a pinned host public key.
    /// # Errors
    /// Refuses an invalid URL or a Noise initialization failure.
    pub fn initiator(
        key: &StaticKey,
        host_key: &[u8; 32],
        version: &VersionTriple,
        url: &str,
        host_id: &str,
    ) -> Result<Self, Error> {
        let prologue = prologue(version, url, host_id)?;
        Ok(Self(
            snow::Builder::with_resolver(SUITE.parse()?, Box::new(Resolver))
                .local_private_key(key.0.expose_secret())?
                .remote_public_key(host_key)?
                .prologue(&prologue)?
                .build_initiator()?,
        ))
    }
    /// Starts the host role. Message 1 alone is never proof of a live runner.
    /// # Errors
    /// Refuses an invalid URL or a Noise initialization failure.
    pub fn responder(
        key: &StaticKey,
        version: &VersionTriple,
        url: &str,
        host_id: &str,
    ) -> Result<Self, Error> {
        let prologue = prologue(version, url, host_id)?;
        Ok(Self(
            snow::Builder::with_resolver(SUITE.parse()?, Box::new(Resolver))
                .local_private_key(key.0.expose_secret())?
                .prologue(&prologue)?
                .build_responder()?,
        ))
    }
    /// Writes the empty payload for this role's handshake message (96 or 48 bytes).
    /// # Errors
    /// Refuses a wrong turn or cryptographic failure; consumes the session.
    pub fn write(mut self) -> Result<(Self, Vec<u8>), Error> {
        let expected = if self.0.is_initiator() { 96 } else { 48 };
        let mut message = vec![0; expected];
        let length = self.0.write_message(&[], &mut message)?;
        if length != expected {
            return Err(Error::Invalid);
        }
        Ok((self, message))
    }
    /// Reads exactly one empty-payload handshake message.
    /// # Errors
    /// Refuses any other size, payload, wrong turn or authentication failure.
    pub fn read(mut self, message: &[u8]) -> Result<Self, Error> {
        let expected = if self.0.is_initiator() { 48 } else { 96 };
        if message.len() != expected {
            return Err(Error::Invalid);
        }
        let length = self.0.read_message(message, &mut [])?;
        if length != 0 {
            return Err(Error::Invalid);
        }
        Ok(self)
    }
    /// Completes IK; the responder must still require a valid first transport message.
    /// # Errors
    /// Refuses an incomplete handshake.
    pub fn finish(self) -> Result<Transport, Error> {
        let mut hash = [0; 32];
        hash.copy_from_slice(self.0.get_handshake_hash());
        let mut remote = [0; 32];
        remote.copy_from_slice(self.0.get_remote_static().ok_or(Error::Invalid)?);
        Ok(Transport {
            state: Some(self.0.into_transport_mode()?),
            hash,
            remote,
        })
    }
}

/// In-order Noise transport. Any failed operation permanently poisons the link.
pub struct Transport {
    state: Option<snow::TransportState>,
    hash: [u8; 32],
    remote: [u8; 32],
}
impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Transport([REDACTED])")
    }
}
impl Transport {
    /// The session binding used by pairing proofs.
    #[must_use]
    pub fn handshake_hash(&self) -> &[u8; 32] {
        &self.hash
    }
    /// The authenticated peer's static public key, not authorization by itself.
    #[must_use]
    pub fn remote_public_key(&self) -> &[u8; 32] {
        &self.remote
    }
    /// Encrypts one bounded transport message.
    /// # Errors
    /// Refuses an oversized message, failed encryption or a poisoned link.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        let mut state = self.state.take().ok_or(Error::Invalid)?;
        if plaintext.len() > crate::MAX_PLAINTEXT_BYTES {
            return Err(Error::Invalid);
        }
        let mut out = vec![0; plaintext.len() + 16];
        let length = state.write_message(plaintext, &mut out)?;
        out.truncate(length);
        self.state = Some(state);
        Ok(out)
    }
    /// Authenticates one in-order message. Replays and reordering destroy the link.
    /// # Errors
    /// Refuses invalid sizes, bad authentication or a poisoned link.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        let mut state = self.state.take().ok_or(Error::Invalid)?;
        if !(16..=65535).contains(&ciphertext.len()) {
            return Err(Error::Invalid);
        }
        let mut out = zeroize::Zeroizing::new(vec![0; ciphertext.len() - 16]);
        let length = state.read_message(ciphertext, &mut out)?;
        out.truncate(length);
        self.state = Some(state);
        Ok(out.to_vec())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn prologue_encoding() -> Result<(), crate::Error> {
        let version = plakous_runner_protocol::VersionTriple::current("rev", "release");
        assert_eq!(super::prologue(&version, "wss://a/", "host")?,
            b"plak-dial/1\x00\x00\x00\x01\x00\x00\x00\x03rev\x00\x00\x00\x07release\x00\x00\x00\x08wss://a/\x00\x00\x00\x04host");
        Ok(())
    }
}
