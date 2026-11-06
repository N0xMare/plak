//! Portable cryptographic and pairing core for runner dial-in; no I/O or trust store.

mod framing;
mod offer;
mod pairing;
mod resolver;
mod session;

pub use framing::{FrameAssembler, MAX_PLAINTEXT_BYTES, split_frame};
pub use offer::Offer;
pub use pairing::{Confirmation, Fingerprint, PairKey, PairProof};
pub use session::{Handshake, StaticKey, Transport};

/// A local failure category. Never send these diagnostics before authentication.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The platform CSPRNG is unavailable.
    #[error("dial entropy unavailable")]
    Entropy(#[source] getrandom::Error),
    /// Noise authentication or state validation failed.
    #[error("dial cryptographic operation failed")]
    Noise(#[source] snow::Error),
    /// The input violates the dial grammar or size bound.
    #[error("invalid dial input")]
    Invalid,
}

impl From<snow::Error> for Error {
    fn from(value: snow::Error) -> Self {
        Self::Noise(value)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests;
