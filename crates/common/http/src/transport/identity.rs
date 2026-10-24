use std::fmt;
use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore};
use sha2::{Digest as _, Sha256};

use crate::{AddressPolicy, ConfigError};

use super::{Limits, TRANSPORT_BEHAVIOR_REVISION, TransportPolicy};

/// Exact non-secret identity supplied with one caller-built TLS configuration.
///
/// Callers must frame every behavior-bearing trust, client-certificate,
/// verifier, provider, and protocol-policy datum. Secret key bytes must never
/// be included; bind their independently managed non-secret generation.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct TlsConfigIdentity([u8; 32]);

impl TlsConfigIdentity {
    /// Hashes one versioned exact caller TLS behavior table.
    ///
    /// # Errors
    ///
    /// Rejects revision zero, which is reserved for unversioned behavior.
    pub fn from_parts(revision: u32, parts: &[&[u8]]) -> Result<Self, ConfigError> {
        if revision == 0 {
            return Err(ConfigError::UnversionedTlsIdentity);
        }
        let mut hasher = framed_identity_hasher(b"plakous-http/caller-tls-identity-v1");
        update_identity_frame(&mut hasher, &revision.to_be_bytes());
        for part in parts {
            update_identity_frame(&mut hasher, part);
        }
        Ok(Self(hasher.finalize().into()))
    }

    /// Returns the stable identity bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for TlsConfigIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TlsConfigIdentity(<non-secret sha256>)")
    }
}

/// Exact immutable transport behavior identity available to attempt hosts.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct TransportBehaviorIdentity([u8; 32]);

impl TransportBehaviorIdentity {
    /// Returns the stable identity bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for TransportBehaviorIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TransportBehaviorIdentity(<non-secret sha256>)")
    }
}

pub(super) fn default_trust_identity() -> TlsConfigIdentity {
    let mut hasher = framed_identity_hasher(b"plakous-http/webpki-ring-safe-defaults-v1");
    for anchor in webpki_roots::TLS_SERVER_ROOTS {
        update_identity_frame(&mut hasher, anchor.subject.as_ref());
        update_identity_frame(&mut hasher, anchor.subject_public_key_info.as_ref());
        match &anchor.name_constraints {
            Some(constraints) => update_identity_frame(&mut hasher, constraints.as_ref()),
            None => update_identity_frame(&mut hasher, b"<no-name-constraints>"),
        }
    }
    TlsConfigIdentity(hasher.finalize().into())
}

pub(super) fn transport_behavior_identity(
    limits: &Limits,
    policy: TransportPolicy,
    trust: TlsConfigIdentity,
    pooled: bool,
    address_policy: Option<AddressPolicy>,
) -> TransportBehaviorIdentity {
    let mut hasher = framed_identity_hasher(b"plakous-http/transport-behavior-v1");
    update_identity_frame(&mut hasher, &TRANSPORT_BEHAVIOR_REVISION.to_be_bytes());
    update_identity_frame(
        &mut hasher,
        match policy {
            TransportPolicy::WebPkiHttp2Preferred => b"webpki-http2-preferred",
            TransportPolicy::WebPkiHttp1Only => b"webpki-http1-only",
            TransportPolicy::CallerTlsHttp2Preferred => b"caller-tls-http2-preferred",
        },
    );
    update_identity_frame(
        &mut hasher,
        if pooled {
            b"pooled-connections"
        } else {
            b"one-shot-connections"
        },
    );
    for value in [
        limits.max_request_headers,
        limits.max_request_header_bytes,
        limits.max_request_body_bytes,
        limits.max_response_headers,
        limits.max_response_header_bytes,
        limits.max_response_chunk_bytes,
        limits.max_response_body_bytes,
    ] {
        update_identity_frame(&mut hasher, &(value as u128).to_be_bytes());
    }
    for duration in [
        limits.resolution_timeout,
        limits.connect_timeout,
        limits.tls_timeout,
        limits.request_write_idle_timeout,
        limits.response_head_timeout,
        limits.response_body_timeout,
    ] {
        update_identity_frame(&mut hasher, &duration.as_secs().to_be_bytes());
        update_identity_frame(&mut hasher, &duration.subsec_nanos().to_be_bytes());
    }
    update_identity_frame(&mut hasher, &trust.as_bytes());
    // Appended only when installed, so clients without a policy keep the
    // identity they had before address policies existed.
    if let Some(address_policy) = address_policy {
        update_identity_frame(&mut hasher, address_policy.identity_label());
    }
    TransportBehaviorIdentity(hasher.finalize().into())
}

/// A hasher already primed with one identity's length-framed domain separator.
/// Every identity in this module is domain-separated the same way, so the
/// separator is framed exactly once, here.
fn framed_identity_hasher(domain: &[u8]) -> Sha256 {
    let mut hasher = Sha256::new();
    update_identity_frame(&mut hasher, domain);
    hasher
}

fn update_identity_frame(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u128).to_be_bytes());
    hasher.update(bytes);
}

pub(super) fn default_tls_config() -> Result<ClientConfig, ConfigError> {
    let roots = webpki_roots::TLS_SERVER_ROOTS
        .iter()
        .cloned()
        .collect::<RootCertStore>();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| ConfigError::Http2LimitTooLarge)
        .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
}
