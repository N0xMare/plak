//! Bounded credential-envelope encoding, version checks and zeroization.

use super::{FORMAT_VERSION, MAX_RECORD_FILE_BYTES, PrivateCredentialStore, PrivateStoreError};
use crate::{
    AccountBinding, CredentialGeneration, CredentialRecord, CredentialSlotId, CredentialStatus,
    CredentialValueError, RecordAge, SecretBytes, SubscriptionIntegrationProfileId,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize as _, Zeroizing};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DiskStatus {
    Ready,
    RefreshRequired,
    RefreshOutcomeUnknown,
    Revoked,
}

impl From<CredentialStatus> for DiskStatus {
    fn from(value: CredentialStatus) -> Self {
        match value {
            CredentialStatus::Ready => Self::Ready,
            CredentialStatus::RefreshRequired => Self::RefreshRequired,
            CredentialStatus::RefreshOutcomeUnknown => Self::RefreshOutcomeUnknown,
            CredentialStatus::Revoked => Self::Revoked,
        }
    }
}

impl From<DiskStatus> for CredentialStatus {
    fn from(value: DiskStatus) -> Self {
        match value {
            DiskStatus::Ready => Self::Ready,
            DiskStatus::RefreshRequired => Self::RefreshRequired,
            DiskStatus::RefreshOutcomeUnknown => Self::RefreshOutcomeUnknown,
            DiskStatus::Revoked => Self::Revoked,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredDocument {
    format_version: u32,
    profile_id: String,
    slot_id: String,
    account_binding: String,
    generation: u64,
    status: DiskStatus,
    payload: String,
}

impl Drop for StoredDocument {
    fn drop(&mut self) {
        self.profile_id.zeroize();
        self.slot_id.zeroize();
        self.account_binding.zeroize();
        self.payload.zeroize();
    }
}

#[derive(Deserialize)]
struct VersionProbe {
    format_version: u32,
}

pub(super) fn encode_record(
    store: &PrivateCredentialStore,
    record: &CredentialRecord,
) -> Result<Zeroizing<Vec<u8>>, PrivateStoreError> {
    let document = StoredDocument {
        format_version: FORMAT_VERSION,
        profile_id: String::from(record.profile_id().as_str()),
        slot_id: String::from(store.slot_id.as_str()),
        account_binding: String::from(record.account_binding().as_str()),
        generation: record.generation().get(),
        status: record.status().into(),
        payload: STANDARD.encode(record.payload().expose_secret()),
    };
    let encoded = serde_json::to_vec(&document).map_err(|_| PrivateStoreError::InvalidRecord)?;
    if u64::try_from(encoded.len()).map_or(true, |length| length > MAX_RECORD_FILE_BYTES) {
        return Err(PrivateStoreError::RecordTooLarge);
    }
    Ok(Zeroizing::new(encoded))
}

pub(super) fn decode_record(
    store: &PrivateCredentialStore,
    bytes: &[u8],
) -> Result<CredentialRecord, PrivateStoreError> {
    let probe: VersionProbe =
        serde_json::from_slice(bytes).map_err(|_| PrivateStoreError::MalformedRecord)?;
    match RecordAge::of_version(probe.format_version.into(), FORMAT_VERSION.into()) {
        RecordAge::Older => return Err(PrivateStoreError::UnsupportedVersion),
        RecordAge::Newer => return Err(PrivateStoreError::WrittenByNewerBuild),
        RecordAge::Current => {}
    }
    let mut document: StoredDocument =
        serde_json::from_slice(bytes).map_err(|_| PrivateStoreError::MalformedRecord)?;
    let slot = CredentialSlotId::new(std::mem::take(&mut document.slot_id))
        .map_err(|_| PrivateStoreError::MalformedRecord)?;
    if slot != store.slot_id {
        return Err(PrivateStoreError::ForeignSlot);
    }
    decode_document(
        &mut document.profile_id,
        &mut document.account_binding,
        document.generation,
        document.status,
        &mut document.payload,
    )
}

fn decode_document(
    profile: &mut String,
    account: &mut String,
    generation: u64,
    status: DiskStatus,
    payload: &mut String,
) -> Result<CredentialRecord, PrivateStoreError> {
    let profile_id = SubscriptionIntegrationProfileId::new(std::mem::take(profile))
        .map_err(|_| PrivateStoreError::MalformedRecord)?;
    let account_binding = AccountBinding::new(std::mem::take(account))
        .map_err(|_| PrivateStoreError::MalformedRecord)?;
    let generation =
        CredentialGeneration::new(generation).map_err(|_| PrivateStoreError::MalformedRecord)?;
    let capacity = payload.len().saturating_mul(3) / 4;
    let mut decoded = Zeroizing::new(Vec::with_capacity(capacity));
    STANDARD
        .decode_vec(payload.as_bytes(), &mut decoded)
        .map_err(|_| PrivateStoreError::MalformedRecord)?;
    if decoded.len() > crate::MAX_CREDENTIAL_PAYLOAD_BYTES {
        return Err(PrivateStoreError::PayloadTooLarge);
    }
    CredentialRecord::new(
        profile_id,
        account_binding,
        generation,
        status.into(),
        SecretBytes::new(std::mem::take(&mut decoded)),
    )
    .map_err(|error| match error {
        CredentialValueError::PayloadTooLarge => PrivateStoreError::PayloadTooLarge,
        _ => PrivateStoreError::MalformedRecord,
    })
}

#[cfg(all(
    test,
    any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd"
    )
))]
#[cfg(test)]
mod tests;
