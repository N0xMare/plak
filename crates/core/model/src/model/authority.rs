//! Runtime model/account authority and its admission witness.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{Digest, DigestDomain, ResolvedModelDescriptor, Timestamp};

/// Maximum credential-free provider admission evidence retained with one
/// runtime authority observation.
///
/// The witness is deliberately small: it may identify an exact provider-owned
/// catalog/capability receipt, but must never contain credentials or arbitrary
/// provider responses.
pub const MAX_RUNTIME_ADMISSION_WITNESS_BYTES: usize = 8 * 1024;

const MAX_RUNTIME_ADMISSION_WITNESS_KIND_BYTES: usize = 128;

/// Bounded opaque provider-owned admission evidence.
///
/// Plakous treats the payload semantically as an exact byte string. The owning
/// adapter defines its canonical codec and must reproduce the same witness on
/// every fresh revalidation. This preserves provider neutrality without
/// flattening provider-specific catalog semantics into the runtime.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct RuntimeAdmissionWitness {
    kind: String,
    revision: u32,
    payload: Vec<u8>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeAdmissionWitnessWire {
    kind: String,
    revision: u32,
    payload: Vec<u8>,
    identity: Digest,
}

impl RuntimeAdmissionWitness {
    /// Seals canonical credential-free provider evidence.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized labels or payloads, a zero revision, or an
    /// identity construction failure.
    pub fn new(
        kind: impl Into<String>,
        revision: u32,
        payload: Vec<u8>,
    ) -> Result<Self, RuntimeModelAuthorityError> {
        let kind = kind.into();
        if revision == 0
            || kind.is_empty()
            || kind.len() > MAX_RUNTIME_ADMISSION_WITNESS_KIND_BYTES
            || !kind.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            })
            || payload.is_empty()
            || payload.len() > MAX_RUNTIME_ADMISSION_WITNESS_BYTES
        {
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        let revision_bytes = revision.to_be_bytes();
        let identity = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-model/runtime-admission-witness-v1",
                kind.as_bytes(),
                &revision_bytes,
                &payload,
            ],
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        Ok(Self {
            kind,
            revision,
            payload,
            identity,
        })
    }

    /// Returns the provider-owned witness kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns the exact canonical witness codec revision.
    #[must_use]
    pub const fn revision(&self) -> u32 {
        self.revision
    }

    /// Returns the exact credential-free canonical payload.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Returns the domain-separated identity of kind, revision, and payload.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for RuntimeAdmissionWitness {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RuntimeAdmissionWitnessWire::deserialize(deserializer)?;
        let witness =
            Self::new(wire.kind, wire.revision, wire.payload).map_err(serde::de::Error::custom)?;
        if witness.identity != wire.identity {
            return Err(serde::de::Error::custom(
                RuntimeModelAuthorityError::Invalid,
            ));
        }
        Ok(witness)
    }
}

impl fmt::Debug for RuntimeAdmissionWitness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeAdmissionWitness")
            .field("kind", &self.kind)
            .field("revision", &self.revision)
            .field("payload", &"<credential-free canonical evidence>")
            .field("identity", &self.identity)
            .finish()
    }
}

/// An owned fresh model/account-authority revalidation operation.
pub type RuntimeModelAuthorityOperation = Pin<
    Box<
        dyn Future<Output = Result<RuntimeModelAuthority, RuntimeModelAuthorityError>>
            + Send
            + 'static,
    >,
>;

/// Fresh credential-free authority facts owned by the selected model adapter.
///
/// The account and credential-authority values are one-way digests. Raw
/// account identifiers, credential references, tokens, and payloads cannot be
/// represented by this value. The agent combines these facts with the actual
/// descriptor, profile, tools, workspace, environment, and effect policy; this
/// value alone grants no dispatch authority.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct RuntimeModelAuthority {
    descriptor_binding: Digest,
    account_binding: Digest,
    credential_authority_binding: Digest,
    credential_generation: u64,
    observed_at: Timestamp,
    valid_until: Option<Timestamp>,
    admission_witness: Option<RuntimeAdmissionWitness>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeModelAuthorityWire {
    descriptor_binding: Digest,
    account_binding: Digest,
    credential_authority_binding: Digest,
    credential_generation: u64,
    observed_at: Timestamp,
    valid_until: Option<Timestamp>,
    admission_witness: Option<RuntimeAdmissionWitness>,
}

impl<'de> Deserialize<'de> for RuntimeModelAuthority {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RuntimeModelAuthorityWire::deserialize(deserializer)?;
        if wire.credential_generation == 0
            || wire.descriptor_binding.domain() != DigestDomain::ModelBinding
            || wire.account_binding.domain() != DigestDomain::ModelBinding
            || wire.credential_authority_binding.domain() != DigestDomain::ModelBinding
            || wire
                .valid_until
                .is_some_and(|until| until <= wire.observed_at)
        {
            return Err(serde::de::Error::custom(
                RuntimeModelAuthorityError::Invalid,
            ));
        }
        Ok(Self {
            descriptor_binding: wire.descriptor_binding,
            account_binding: wire.account_binding,
            credential_authority_binding: wire.credential_authority_binding,
            credential_generation: wire.credential_generation,
            observed_at: wire.observed_at,
            valid_until: wire.valid_until,
            admission_witness: wire.admission_witness,
        })
    }
}

impl RuntimeModelAuthority {
    /// Creates adapter-owned, credential-free fresh authority evidence.
    ///
    /// # Errors
    ///
    /// Returns a typed error for a zero generation, non-model-binding digest,
    /// invalid freshness window, or descriptor whose binding cannot be
    /// computed.
    pub fn new(
        descriptor: &ResolvedModelDescriptor,
        account_binding: Digest,
        credential_authority_binding: Digest,
        credential_generation: u64,
        observed_at: Timestamp,
        valid_until: Option<Timestamp>,
    ) -> Result<Self, RuntimeModelAuthorityError> {
        if credential_generation == 0
            || account_binding.domain() != DigestDomain::ModelBinding
            || credential_authority_binding.domain() != DigestDomain::ModelBinding
            || valid_until.is_some_and(|until| until <= observed_at)
        {
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        let descriptor_binding = descriptor
            .binding_digest()
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        Ok(Self {
            descriptor_binding,
            account_binding,
            credential_authority_binding,
            credential_generation,
            observed_at,
            valid_until,
            admission_witness: None,
        })
    }

    /// Attaches exact bounded provider-owned admission evidence.
    #[must_use]
    pub fn with_admission_witness(mut self, witness: RuntimeAdmissionWitness) -> Self {
        self.admission_witness = Some(witness);
        self
    }

    /// Validates this evidence against the actual selected descriptor and time.
    ///
    /// # Errors
    ///
    /// Returns a typed mismatch or stale-evidence error without exposing
    /// account or credential metadata.
    pub fn validate(
        &self,
        descriptor: &ResolvedModelDescriptor,
        checked_at: Timestamp,
    ) -> Result<(), RuntimeModelAuthorityError> {
        let binding = descriptor
            .binding_digest()
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        if binding != self.descriptor_binding {
            return Err(RuntimeModelAuthorityError::ForeignDescriptor);
        }
        if self.observed_at > checked_at
            || self
                .valid_until
                .is_some_and(|valid_until| valid_until <= checked_at)
        {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        Ok(())
    }

    /// Returns the exact selected-descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }

    /// Returns the one-way selected-account binding.
    #[must_use]
    pub const fn account_binding(&self) -> Digest {
        self.account_binding
    }

    /// Returns the one-way credential-source/authority binding.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }

    /// Returns the stable account/credential-authority class used by
    /// descriptor-owned capacity and measurement certificates.
    ///
    /// This deliberately excludes observation time, freshness, and credential
    /// generation. The complete execution authority identity retains those
    /// axes independently and still binds every prepared call.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated identity cannot be built.
    pub fn authority_class_binding(&self) -> Result<Digest, RuntimeModelAuthorityError> {
        Self::authority_class_for(self.account_binding, self.credential_authority_binding)
    }

    /// Derives the stable capacity/measurement certificate scope from exact
    /// non-secret account and credential-authority bindings.
    ///
    /// # Errors
    ///
    /// Returns an error if either input has the wrong digest domain or the
    /// domain-separated class identity cannot be constructed.
    pub fn authority_class_for(
        account_binding: Digest,
        credential_authority_binding: Digest,
    ) -> Result<Digest, RuntimeModelAuthorityError> {
        if account_binding.domain() != DigestDomain::ModelBinding
            || credential_authority_binding.domain() != DigestDomain::ModelBinding
        {
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        Digest::sha256(
            DigestDomain::ModelBinding,
            3,
            &[
                b"plakous-model/runtime-authority-class-v1",
                &account_binding.identity_bytes(),
                &credential_authority_binding.identity_bytes(),
            ],
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)
    }

    /// Returns the exact nonzero credential generation.
    #[must_use]
    pub const fn credential_generation(&self) -> u64 {
        self.credential_generation
    }

    /// Returns when the owning adapter observed these facts.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    /// Returns the exclusive freshness deadline, when bounded.
    #[must_use]
    pub const fn valid_until(&self) -> Option<Timestamp> {
        self.valid_until
    }

    /// Returns exact provider-owned admission evidence, when the selected
    /// route requires an additional catalog/capability fence.
    #[must_use]
    pub const fn admission_witness(&self) -> Option<&RuntimeAdmissionWitness> {
        self.admission_witness.as_ref()
    }

    /// Returns the stable exact selected authority used by prepared execution.
    ///
    /// Observation and expiry time are freshness evidence, not lowering inputs,
    /// and are intentionally excluded. Account, credential authority, and exact
    /// credential generation remain bound.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated identity cannot be built.
    pub fn execution_authority_binding(&self) -> Result<Digest, RuntimeModelAuthorityError> {
        let generation = self.credential_generation.to_be_bytes();
        let witness = self
            .admission_witness
            .as_ref()
            .map_or_else(Vec::new, |witness| witness.identity().identity_bytes());
        Digest::sha256(
            DigestDomain::ModelBinding,
            5,
            &[
                b"plakous-model/stable-runtime-model-authority-v2",
                &self.descriptor_binding.identity_bytes(),
                &self.account_binding.identity_bytes(),
                &self.credential_authority_binding.identity_bytes(),
                &generation,
                &witness,
            ],
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)
    }

    /// Alias for the clock-independent exact execution-authority binding.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated identity cannot be built.
    pub fn stable_identity(&self) -> Result<Digest, RuntimeModelAuthorityError> {
        self.execution_authority_binding()
    }

    /// Reports whether two fresh observations name the same exact selected
    /// descriptor, account, credential authority, and generation.
    #[must_use]
    pub fn same_selected_authority(&self, other: &Self) -> bool {
        self.descriptor_binding == other.descriptor_binding
            && self.account_binding == other.account_binding
            && self.credential_authority_binding == other.credential_authority_binding
            && self.credential_generation == other.credential_generation
            && self.admission_witness == other.admission_witness
    }
}

impl fmt::Debug for RuntimeModelAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeModelAuthority")
            .field("descriptor_binding", &self.descriptor_binding)
            .field("account_binding", &"<redacted digest>")
            .field("credential_authority_binding", &"<redacted digest>")
            .field("credential_generation", &self.credential_generation)
            .field("observed_at", &self.observed_at)
            .field("valid_until", &self.valid_until)
            .field("admission_witness", &self.admission_witness)
            .finish()
    }
}

/// Typed redaction-safe failure to produce fresh model/account authority.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RuntimeModelAuthorityError {
    /// The adapter has no honest authority producer for this route.
    #[error("fresh model/account authority is unavailable")]
    Unavailable,
    /// The adapter returned malformed or contradictory authority facts.
    #[error("fresh model/account authority is invalid")]
    Invalid,
    /// The authority facts belong to another selected descriptor.
    #[error("fresh model/account authority belongs to another descriptor")]
    ForeignDescriptor,
    /// Fresh facts name another account or credential generation for this descriptor.
    #[error("fresh model/account authority belongs to another authority generation")]
    ForeignAuthority,
    /// The evidence is future-dated or no longer current.
    #[error("fresh model/account authority is stale")]
    Stale,
}
