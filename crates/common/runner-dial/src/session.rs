use crate::{Error, resolver::Resolver};
use plakous_runner_protocol::VersionTriple;
use secrecy::{ExposeSecret, ExposeSecretMut, SecretBox};
use std::{fmt, marker::PhantomData};

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
        let mut secret = SecretBox::<[u8; 32]>::default();
        getrandom::fill(secret.expose_secret_mut()).map_err(Error::Entropy)?;
        Ok(Self(secret))
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

/// The runner's initiating handshake role.
pub enum Initiator {}
/// The host's responding handshake role.
pub enum Responder {}

/// One role-typed IK handshake. Consuming operations prevent reuse after failure.
pub struct Handshake<Role>(snow::HandshakeState, PhantomData<Role>);
impl<Role> fmt::Debug for Handshake<Role> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Handshake([REDACTED])")
    }
}

fn prologue(version: &VersionTriple, url: &str, host_id: &str) -> Result<Vec<u8>, Error> {
    super::offer::validate_url(url)?;
    if host_id.is_empty() {
        return Err(Error::Invalid);
    }
    let mut bytes = b"plak-dial/1".to_vec();
    bytes.extend_from_slice(&version.protocol.to_be_bytes());
    // Length prefixes make the variable-width concatenation unambiguous.
    for field in [url, host_id] {
        let length = u32::try_from(field.len()).map_err(|_| Error::Invalid)?;
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    Ok(bytes)
}

impl Handshake<Initiator> {
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
            PhantomData,
        ))
    }
    /// Completes the initiator's handshake.
    /// # Errors
    /// Refuses an incomplete handshake.
    pub fn finish(self) -> Result<Transport, Error> {
        self.into_transport()
    }
}

impl Handshake<Responder> {
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
            PhantomData,
        ))
    }
    /// Completes IK without exposing the replayable runner identity or a writer.
    /// # Errors
    /// Refuses an incomplete handshake.
    pub fn finish(self) -> Result<PendingTransport, Error> {
        Ok(PendingTransport(self.into_transport()?))
    }
}

impl<Role> Handshake<Role> {
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
    fn into_transport(self) -> Result<Transport, Error> {
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

/// A responder waiting for cryptographic key confirmation, not authorization.
/// No peer identity, handshake hash or encryption method is available yet.
///
/// ```compile_fail
/// fn premature_identity(pending: plakous_runner_dial::PendingTransport) {
///     pending.remote_public_key();
/// }
/// ```
///
/// ```compile_fail
/// fn premature_write(mut pending: plakous_runner_dial::PendingTransport) {
///     pending.encrypt(b"unauthorized");
/// }
/// ```
pub struct PendingTransport(Transport);
impl fmt::Debug for PendingTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingTransport([REDACTED])")
    }
}
impl PendingTransport {
    /// Decrypts the first runner message before releasing the peer identity.
    /// The caller must still validate its `PairProof` or Hello before authorizing it.
    /// # Errors
    /// Consumes the pending session on a size or authentication failure.
    pub fn confirm(mut self, first_ciphertext: &[u8]) -> Result<(Transport, Vec<u8>), Error> {
        let plaintext = self.0.decrypt(first_ciphertext)?;
        Ok((self.0, plaintext))
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
        assert_eq!(
            super::prologue(&version, "wss://a/", "host")?,
            b"plak-dial/1\x00\x00\x00\x05\x00\x00\x00\x08wss://a/\x00\x00\x00\x04host"
        );
        Ok(())
    }
}
