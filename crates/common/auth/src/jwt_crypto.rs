//! Verification-only JWT backend sharing the transport's ring crypto stack.

use std::sync::OnceLock;

use jsonwebtoken::crypto::{CryptoProvider, JwtVerifier, KeyUtils};
use jsonwebtoken::errors::{ErrorKind, Result};
use jsonwebtoken::signature::{Error, Verifier};
use jsonwebtoken::{Algorithm, AlgorithmFamily, DecodingKey, DecodingKeyKind};
use ring::signature::{ECDSA_P256_SHA256_FIXED, RSA_PKCS1_2048_8192_SHA256};

static PROVIDER: CryptoProvider = CryptoProvider {
    signer_factory: |_, _| Err(ErrorKind::InvalidAlgorithm.into()),
    verifier_factory: |algorithm, key| verifier(*algorithm, key),
    key_utils: KEY_UTILS,
};

const KEY_UTILS: KeyUtils = KeyUtils {
    rsa_pub_components_from_private_key: |_| Err(ErrorKind::InvalidKeyFormat.into()),
    rsa_pub_components_from_public_key: |_| Err(ErrorKind::InvalidKeyFormat.into()),
    ec_pub_components_from_private_key: |_, _| Err(ErrorKind::InvalidKeyFormat.into()),
    ed_pub_components_from_private_key: |_, _| Err(ErrorKind::InvalidKeyFormat.into()),
    compute_digest: |_, _| Err(ErrorKind::InvalidAlgorithm.into()),
};

pub(crate) fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Only a provider explicitly installed before this first decode wins.
        let _ = PROVIDER.install_default();
    });
}

fn verifier(algorithm: Algorithm, key: &DecodingKey) -> Result<Box<dyn JwtVerifier>> {
    match (algorithm, key.family()) {
        (Algorithm::RS256, AlgorithmFamily::Rsa) | (Algorithm::ES256, AlgorithmFamily::Ec) => {
            Ok(Box::new(RingVerifier {
                algorithm,
                key: key.clone(),
            }))
        }
        _ => Err(ErrorKind::InvalidAlgorithm.into()),
    }
}

struct RingVerifier {
    algorithm: Algorithm,
    key: DecodingKey,
}

impl JwtVerifier for RingVerifier {
    fn algorithm(&self) -> Algorithm {
        self.algorithm
    }
}

impl Verifier<Vec<u8>> for RingVerifier {
    fn verify(&self, message: &[u8], signature: &Vec<u8>) -> std::result::Result<(), Error> {
        let verified = match (self.algorithm, self.key.kind()) {
            (Algorithm::RS256, DecodingKeyKind::RsaModulusExponent { n, e }) => {
                ring::signature::RsaPublicKeyComponents { n, e }.verify(
                    &RSA_PKCS1_2048_8192_SHA256,
                    message,
                    signature,
                )
            }
            (Algorithm::RS256, DecodingKeyKind::SecretOrDer(bytes)) => {
                ring::signature::UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, bytes)
                    .verify(message, signature)
            }
            (Algorithm::ES256, DecodingKeyKind::SecretOrDer(bytes)) => {
                ring::signature::UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, bytes)
                    .verify(message, signature)
            }
            _ => return Err(Error::new()),
        };
        verified.map_err(|_| Error::new())
    }
}

#[cfg(test)]
mod tests;
