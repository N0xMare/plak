#[cfg(unix)]
use rustix::fs::{FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::File;
#[cfg(unix)]
use std::io::Read as _;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt as _;
#[cfg(all(unix, test))]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use plakous_auth::RecordAge;

use super::{
    CompatibilityNotice, CompatibilityStatus, NativeAuthStatus, NativeModelCatalog,
    NativeModelSummary, NativeProvider, NativeSubscriptionError, catalog, fmt, run_blocking,
};
mod filesystem;
mod locks;
pub(super) use locks::PrivateFilesystemLock;
use locks::{acquire_private_filesystem_lock, try_acquire_private_filesystem_lock};
mod quarantine;
use quarantine::{
    LOGIN_RECOVERY_QUARANTINE, clear_quarantine, process_quarantine_key, quarantine, quarantined,
};
mod recovery;
#[cfg(all(unix, test))]
pub(crate) use filesystem::AtomicWriteStage;
use filesystem::{
    PrivateFileIdentity, ensure_state_root, existing_layout, open_or_create_private_child,
    open_private_parent, private_file_identity, read_private_bounded,
    read_private_bounded_with_hook_and_limit, remove_private_file_durable,
    validate_private_directory, validate_private_file, validate_private_file_metadata,
    validate_state_root, write_private_atomic, write_private_atomic_with_hook,
};
use recovery::{
    arm_login_recovery, clear_login_recovery, current_credential_generation,
    login_recovery_required_for_status, read_login_recovery_marker, rebind_login_recovery_consent,
    repin_cleared_login_recovery, restore_login_recovery, retire_login_recovery,
};

const RECEIPT_SCHEMA: u32 = 2;
const MAX_RECEIPT_BYTES: u64 = 64 * 1024;
const LOGIN_RECOVERY_SCHEMA: &str = "plak.native-login-recovery";
const LOGIN_RECOVERY_VERSION: u32 = 2;
const CONSENT_GENERATION_BYTES: usize = 32;
const LOGIN_RECOVERY_NONCE_BYTES: usize = 32;
const MAX_CREDENTIAL_RECORD_BYTES: u64 = 384 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LoginRecoveryState {
    Armed,
    Cleared,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LoginRecoveryMarker {
    schema: String,
    version: u32,
    state: LoginRecoveryState,
    operation_nonce: String,
    consent_generation: String,
    credential_generation: Option<u64>,
}

#[derive(Clone)]
pub(super) struct LoginRecoveryOperation {
    marker: LoginRecoveryMarker,
    previous: Option<Vec<u8>>,
    previous_epoch: Option<Vec<u8>>,
    pub(super) credential_generation_before: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LoginRecoveryEpoch {
    schema: String,
    version: u32,
    state: LoginRecoveryState,
    operation_nonce: String,
    consent_generation: String,
    credential_generation: Option<u64>,
}

/// Fixed Plak-owned storage derived from one explicit XDG state root.
///
/// No environment, home directory, external login store, or provider CLI path
/// is consulted by this type.
#[derive(Clone)]
pub struct SubscriptionPaths {
    state_root: PathBuf,
}

impl SubscriptionPaths {
    /// Binds the fixed Plak layout to one explicit absolute XDG state root.
    ///
    /// # Errors
    ///
    /// Rejects relative, empty, parent-traversing, or root-only paths.
    pub fn from_xdg_state_home(
        state_root: impl Into<PathBuf>,
    ) -> Result<Self, NativeSubscriptionError> {
        let state_root = state_root.into();
        validate_state_root(&state_root)?;
        Ok(Self { state_root })
    }

    fn claude_client_device_id(
        &self,
    ) -> Result<plakous_anthropic::ClaudeClientDeviceId, NativeSubscriptionError> {
        self.prepare()?;
        let path = self.layout_root().join("client-device-id");
        let lock =
            acquire_private_filesystem_lock(&self.layout_root().join(".client-device-id.lock"))?;
        lock.verify()?;
        let value = match read_private_bounded(&path) {
            Ok(bytes) => {
                String::from_utf8(bytes).map_err(|_| NativeSubscriptionError::UnsafeStorage)?
            }
            Err(NativeSubscriptionError::AcceptanceRequired) => {
                let value = new_random_identity()?;
                write_private_atomic(&path, value.as_bytes())?;
                value
            }
            Err(error) => return Err(error),
        };
        lock.verify()?;
        plakous_anthropic::ClaudeClientDeviceId::new(value)
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)
    }

    fn provider_record(&self, provider: NativeProvider) -> PathBuf {
        self.layout_root()
            .join(provider.as_str())
            .join("credential.json")
    }

    fn receipt_path(&self, provider: NativeProvider) -> PathBuf {
        self.layout_root()
            .join("compatibility")
            .join(format!("{}.json", provider.as_str()))
    }

    fn compatibility_lock_path(&self, provider: NativeProvider) -> PathBuf {
        self.layout_root()
            .join("compatibility")
            .join(format!(".{}.lock", provider.as_str()))
    }

    fn login_recovery_path(&self, provider: NativeProvider) -> PathBuf {
        self.layout_root()
            .join("compatibility")
            .join(format!("{}.login-recovery", provider.as_str()))
    }

    fn login_recovery_epoch_path(&self, provider: NativeProvider) -> PathBuf {
        self.layout_root()
            .join("compatibility")
            .join(format!("{}.login-recovery-epoch", provider.as_str()))
    }

    fn catalog_snapshot_path(&self, provider: NativeProvider) -> PathBuf {
        self.layout_root()
            .join("catalog")
            .join(format!("{}.json", provider.as_str()))
    }

    fn layout_root(&self) -> PathBuf {
        self.state_root.join("plak/subscriptions")
    }

    fn prepare(&self) -> Result<(), NativeSubscriptionError> {
        let state = ensure_state_root(&self.state_root)?;
        let plak = open_or_create_private_child(&state, OsStr::new("plak"))?;
        let subscriptions = open_or_create_private_child(&plak, OsStr::new("subscriptions"))?;
        for name in ["codex", "xai", "claude", "catalog", "compatibility"] {
            let _ = open_or_create_private_child(&subscriptions, OsStr::new(name))?;
        }
        Ok(())
    }
}

impl fmt::Debug for SubscriptionPaths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SubscriptionPaths(<redacted explicit state root>)")
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptanceReceipt {
    schema: u32,
    provider: String,
    profile_id: String,
    source_revision: String,
    secondary_source_revision: Option<String>,
    notice_revision: String,
    provenance: String,
    reviewed_at: u64,
    expires_at: u64,
    kill_switch_revision: String,
    accepted_at: u64,
    generation: String,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct LoadedAcceptance {
    pub(super) accepted_at: u64,
    pub(super) exact_bytes: Vec<u8>,
    pub(super) generation: String,
}

impl AcceptanceReceipt {
    fn new(notice: CompatibilityNotice, accepted_at: u64, generation: String) -> Self {
        Self {
            schema: RECEIPT_SCHEMA,
            provider: notice.provider.as_str().to_owned(),
            profile_id: notice.profile_id.to_owned(),
            source_revision: notice.source_revision.to_owned(),
            secondary_source_revision: notice.secondary_source_revision.map(str::to_owned),
            notice_revision: notice.notice_revision.to_owned(),
            provenance: notice.provenance.to_owned(),
            reviewed_at: notice.reviewed_at,
            expires_at: notice.expires_at,
            kill_switch_revision: notice.kill_switch_revision.to_owned(),
            accepted_at,
            generation,
        }
    }

    fn validate(
        &self,
        notice: CompatibilityNotice,
        now: u64,
    ) -> Result<u64, NativeSubscriptionError> {
        if self.schema != RECEIPT_SCHEMA
            || self.provider != notice.provider.as_str()
            || self.profile_id != notice.profile_id
            || self.source_revision != notice.source_revision
            || self.secondary_source_revision.as_deref() != notice.secondary_source_revision
            || self.notice_revision != notice.notice_revision
            || self.provenance != notice.provenance
            || self.reviewed_at != notice.reviewed_at
            || self.expires_at != notice.expires_at
            || self.kill_switch_revision != notice.kill_switch_revision
            || !valid_generation(&self.generation)
        {
            return Err(NativeSubscriptionError::AcceptanceRequired);
        }
        validate_notice_time(notice, self.accepted_at, now)?;
        Ok(self.accepted_at)
    }
}

fn validate_notice_time(
    notice: CompatibilityNotice,
    accepted_at: u64,
    now: u64,
) -> Result<(), NativeSubscriptionError> {
    validate_profile_time(notice, now)?;
    if accepted_at < notice.reviewed_at || accepted_at >= notice.expires_at || accepted_at > now {
        return Err(NativeSubscriptionError::AcceptanceRequired);
    }
    Ok(())
}

fn validate_profile_time(
    notice: CompatibilityNotice,
    now: u64,
) -> Result<(), NativeSubscriptionError> {
    if notice.killed || now < notice.reviewed_at || now >= notice.expires_at {
        return Err(NativeSubscriptionError::ProfileInactive);
    }
    Ok(())
}

/// Writes a fresh acceptance receipt and returns its new consent generation.
fn store_acceptance(
    paths: &SubscriptionPaths,
    notice: CompatibilityNotice,
    accepted_at: u64,
) -> Result<String, NativeSubscriptionError> {
    paths.prepare()?;
    validate_notice_time(notice, accepted_at, accepted_at)?;
    let generation = new_random_identity()?;
    let bytes = serde_json::to_vec(&AcceptanceReceipt::new(
        notice,
        accepted_at,
        generation.clone(),
    ))
    .map_err(|_| NativeSubscriptionError::InvalidReceipt)?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(NativeSubscriptionError::InvalidReceipt);
    }
    write_private_atomic(&paths.receipt_path(notice.provider), &bytes)?;
    Ok(generation)
}

/// The consent generation of the stored receipt whether or not it is current
/// for this build's notice. A login stays bound to the acceptance it was made
/// under even after a profile re-review makes that acceptance stale; only an
/// absent or unreadable receipt has no generation.
fn stored_acceptance_generation(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
) -> Result<Option<String>, NativeSubscriptionError> {
    let bytes = match read_private_bounded(&paths.receipt_path(provider)) {
        Ok(bytes) => bytes,
        Err(NativeSubscriptionError::AcceptanceRequired) => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(serde_json::from_slice::<AcceptanceReceipt>(&bytes)
        .ok()
        .filter(|receipt| {
            receipt.schema == RECEIPT_SCHEMA
                && receipt.provider == provider.as_str()
                && valid_generation(&receipt.generation)
        })
        .map(|receipt| receipt.generation))
}

fn load_acceptance(
    paths: &SubscriptionPaths,
    notice: CompatibilityNotice,
    now: u64,
) -> Result<u64, NativeSubscriptionError> {
    load_acceptance_exact(paths, notice, now).map(|receipt| receipt.accepted_at)
}

fn load_acceptance_exact(
    paths: &SubscriptionPaths,
    notice: CompatibilityNotice,
    now: u64,
) -> Result<LoadedAcceptance, NativeSubscriptionError> {
    paths.prepare()?;
    validate_profile_time(notice, now)?;
    let bytes = read_private_bounded(&paths.receipt_path(notice.provider))?;
    let receipt: AcceptanceReceipt =
        serde_json::from_slice(&bytes).map_err(|_| NativeSubscriptionError::InvalidReceipt)?;
    let accepted_at = receipt.validate(notice, now)?;
    Ok(LoadedAcceptance {
        accepted_at,
        exact_bytes: bytes,
        generation: receipt.generation,
    })
}

/// The version fields of a receipt, read without its strict shape.
#[derive(Deserialize)]
struct ReceiptVersionProbe {
    schema: u32,
    profile_id: String,
}

/// The version fields of a login-recovery marker or epoch.
#[derive(Deserialize)]
struct LoginRecoveryVersionProbe {
    schema: String,
    version: u32,
}

/// Whether a newer build wrote this provider's compatibility receipt: a
/// newer receipt schema, or this schema under a newer compatibility profile.
/// An absent or unparseable receipt is not newer; its readers judge it.
fn receipt_written_by_newer_build(
    paths: &SubscriptionPaths,
    notice: CompatibilityNotice,
) -> Result<bool, NativeSubscriptionError> {
    let bytes = match read_private_bounded(&paths.receipt_path(notice.provider)) {
        Ok(bytes) => bytes,
        Err(NativeSubscriptionError::AcceptanceRequired) => return Ok(false),
        Err(error) => return Err(error),
    };
    let Ok(probe) = serde_json::from_slice::<ReceiptVersionProbe>(&bytes) else {
        return Ok(false);
    };
    Ok(
        match RecordAge::of_version(probe.schema.into(), RECEIPT_SCHEMA.into()) {
            RecordAge::Newer => true,
            RecordAge::Older => false,
            RecordAge::Current => {
                match (
                    plakous_auth::SubscriptionIntegrationProfileId::new(probe.profile_id),
                    plakous_auth::SubscriptionIntegrationProfileId::new(notice.profile_id),
                ) {
                    (Ok(stored), Ok(current)) => stored.age_against(&current) == RecordAge::Newer,
                    _ => false,
                }
            }
        },
    )
}

/// Whether a newer build wrote this provider's login-recovery marker or
/// its epoch (this schema at a newer version).
fn login_recovery_written_by_newer_build(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
) -> Result<bool, NativeSubscriptionError> {
    for path in [
        paths.login_recovery_path(provider),
        paths.login_recovery_epoch_path(provider),
    ] {
        let bytes = match read_private_bounded(&path) {
            Ok(bytes) => bytes,
            Err(NativeSubscriptionError::AcceptanceRequired) => continue,
            Err(error) => return Err(error),
        };
        if serde_json::from_slice::<LoginRecoveryVersionProbe>(&bytes).is_ok_and(|probe| {
            probe.schema == LOGIN_RECOVERY_SCHEMA
                && RecordAge::of_version(probe.version.into(), LOGIN_RECOVERY_VERSION.into())
                    == RecordAge::Newer
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Removes a newer build's receipt; only after the operator confirmed
/// replacing that build's login state.
fn retire_newer_receipt(
    paths: &SubscriptionPaths,
    notice: CompatibilityNotice,
) -> Result<(), NativeSubscriptionError> {
    if receipt_written_by_newer_build(paths, notice)? {
        remove_private_file_durable(&paths.receipt_path(notice.provider))?;
    }
    Ok(())
}

fn valid_nonce(value: &str) -> bool {
    value.len() == LOGIN_RECOVERY_NONCE_BYTES * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(unix)]
fn new_random_identity() -> Result<String, NativeSubscriptionError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let descriptor = rustix::fs::open(
        "/dev/urandom",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| NativeSubscriptionError::Internal)?;
    let mut random = File::from(descriptor);
    let metadata = random
        .metadata()
        .map_err(|_| NativeSubscriptionError::Internal)?;
    if !metadata.file_type().is_char_device() {
        return Err(NativeSubscriptionError::Internal);
    }
    let mut bytes = [0_u8; CONSENT_GENERATION_BYTES];
    random
        .read_exact(&mut bytes)
        .map_err(|_| NativeSubscriptionError::Internal)?;
    let mut generation = String::with_capacity(CONSENT_GENERATION_BYTES * 2);
    for byte in bytes {
        generation.push(char::from(HEX[usize::from(byte >> 4)]));
        generation.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(generation)
}

#[cfg(not(unix))]
fn new_random_identity() -> Result<String, NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

fn valid_generation(value: &str) -> bool {
    value.len() == CONSENT_GENERATION_BYTES * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone)]
pub(super) struct SubscriptionState {
    paths: SubscriptionPaths,
}

impl SubscriptionState {
    pub(in crate::subscriptions) async fn acquire_compatibility_lock(
        &self,
        provider: NativeProvider,
    ) -> Result<PrivateFilesystemLock, NativeSubscriptionError> {
        let paths = self.paths.clone();
        run_blocking(move || {
            paths.prepare()?;
            acquire_private_filesystem_lock(&paths.compatibility_lock_path(provider))
        })
        .await?
    }

    pub(in crate::subscriptions) async fn try_acquire_compatibility_lock(
        &self,
        provider: NativeProvider,
    ) -> Result<Option<PrivateFilesystemLock>, NativeSubscriptionError> {
        let paths = self.paths.clone();
        run_blocking(move || {
            paths.prepare()?;
            try_acquire_private_filesystem_lock(&paths.compatibility_lock_path(provider))
        })
        .await?
    }
}

impl SubscriptionState {
    pub(super) const fn new(paths: SubscriptionPaths) -> Self {
        Self { paths }
    }
}

impl SubscriptionState {
    pub(super) fn load_acceptance(
        &self,
        notice: CompatibilityNotice,
        now: u64,
    ) -> Result<u64, NativeSubscriptionError> {
        load_acceptance(&self.paths, notice, now)
    }
    pub(super) fn load_acceptance_exact(
        &self,
        notice: CompatibilityNotice,
        now: u64,
    ) -> Result<LoadedAcceptance, NativeSubscriptionError> {
        load_acceptance_exact(&self.paths, notice, now)
    }
    pub(super) fn store_acceptance(
        &self,
        notice: CompatibilityNotice,
        accepted_at: u64,
    ) -> Result<String, NativeSubscriptionError> {
        store_acceptance(&self.paths, notice, accepted_at)
    }
    pub(super) fn stored_acceptance_generation(
        &self,
        provider: NativeProvider,
    ) -> Result<Option<String>, NativeSubscriptionError> {
        stored_acceptance_generation(&self.paths, provider)
    }
    pub(super) fn rebind_login_recovery_consent(
        &self,
        provider: NativeProvider,
        previous: &str,
        next: &str,
    ) -> Result<(), NativeSubscriptionError> {
        rebind_login_recovery_consent(&self.paths, provider, previous, next)
    }
    pub(super) fn arm_login_recovery(
        &self,
        provider: NativeProvider,
        consent_generation: &str,
    ) -> Result<LoginRecoveryOperation, NativeSubscriptionError> {
        arm_login_recovery(&self.paths, provider, consent_generation)
    }
    pub(super) fn clear_login_recovery(
        &self,
        provider: NativeProvider,
        operation: &LoginRecoveryOperation,
        credential_generation: u64,
    ) -> Result<(), NativeSubscriptionError> {
        clear_login_recovery(&self.paths, provider, operation, credential_generation)
    }
    pub(super) fn restore_login_recovery(
        &self,
        provider: NativeProvider,
        operation: &LoginRecoveryOperation,
    ) -> Result<(), NativeSubscriptionError> {
        restore_login_recovery(&self.paths, provider, operation)
    }
    pub(super) fn retire_login_recovery(
        &self,
        provider: NativeProvider,
    ) -> Result<(), NativeSubscriptionError> {
        retire_login_recovery(&self.paths, provider)
    }
    /// Whether a newer build wrote this provider's receipt or recovery state.
    pub(super) fn compatibility_written_by_newer_build(
        &self,
        notice: CompatibilityNotice,
    ) -> Result<bool, NativeSubscriptionError> {
        Ok(receipt_written_by_newer_build(&self.paths, notice)?
            || login_recovery_written_by_newer_build(&self.paths, notice.provider)?)
    }
    pub(super) fn receipt_written_by_newer_build(
        &self,
        notice: CompatibilityNotice,
    ) -> Result<bool, NativeSubscriptionError> {
        receipt_written_by_newer_build(&self.paths, notice)
    }
    pub(super) fn retire_newer_receipt(
        &self,
        notice: CompatibilityNotice,
    ) -> Result<(), NativeSubscriptionError> {
        retire_newer_receipt(&self.paths, notice)
    }
    pub(super) fn current_credential_generation(
        &self,
        provider: NativeProvider,
    ) -> Result<Option<u64>, NativeSubscriptionError> {
        current_credential_generation(&self.paths, provider)
    }
    pub(super) fn login_recovery_required_for_status(
        &self,
        provider: NativeProvider,
        consent_generation: Option<&str>,
        credential: NativeAuthStatus,
    ) -> Result<bool, NativeSubscriptionError> {
        login_recovery_required_for_status(&self.paths, provider, consent_generation, credential)
    }
    pub(super) fn prepare(&self) -> Result<(), NativeSubscriptionError> {
        self.paths.prepare()
    }
    pub(super) fn layout_exists(&self) -> Result<bool, NativeSubscriptionError> {
        existing_layout(&self.paths.state_root)
    }
    /// Compatibility of a provider whose storage does not exist: no receipt
    /// can be current, so only the pinned profile's own window decides.
    pub(super) fn absent_compatibility(
        notice: CompatibilityNotice,
        now: u64,
    ) -> CompatibilityStatus {
        match validate_profile_time(notice, now) {
            Ok(()) => CompatibilityStatus::Required,
            Err(_) => CompatibilityStatus::Inactive,
        }
    }
    pub(super) fn provider_record(&self, provider: NativeProvider) -> PathBuf {
        self.paths.provider_record(provider)
    }
    pub(super) fn claude_client_device_id(
        &self,
    ) -> Result<plakous_anthropic::ClaudeClientDeviceId, NativeSubscriptionError> {
        self.paths.claude_client_device_id()
    }
}

impl SubscriptionState {
    pub(super) fn repin_after_refresh(
        &self,
        provider: NativeProvider,
        consent_generation: &str,
    ) -> Result<(), NativeSubscriptionError> {
        let Some((marker, _)) = read_login_recovery_marker(&self.paths, provider)? else {
            return Ok(());
        };
        if marker.state != LoginRecoveryState::Cleared {
            return Ok(());
        }
        if Some(marker.consent_generation.as_str()) != Some(consent_generation) {
            return Ok(());
        }
        let Some(current) = self.current_credential_generation(provider)? else {
            return Ok(());
        };
        match marker.credential_generation {
            Some(pinned) if current > pinned => {
                repin_cleared_login_recovery(&self.paths, provider, &marker, current)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;

impl SubscriptionState {
    pub(super) fn load_persisted_catalog(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        let bytes = match read_private_bounded_with_hook_and_limit(
            &self.paths.catalog_snapshot_path(provider),
            catalog::MAX_PERSISTED_CATALOG_BYTES,
            || {},
        ) {
            Err(NativeSubscriptionError::AcceptanceRequired) => {
                return Err(NativeSubscriptionError::ModelUnavailable);
            }
            other => other?,
        };
        catalog::decode_persisted_catalog(&bytes, provider)
    }
    pub(super) fn persist_catalog_snapshot(
        &self,
        provider: NativeProvider,
        models: &[NativeModelSummary],
    ) -> Result<(), NativeSubscriptionError> {
        let bytes = catalog::encode_catalog_snapshot(provider, models)?;
        self.paths.prepare()?;
        write_private_atomic(&self.paths.catalog_snapshot_path(provider), &bytes)
    }
}

impl SubscriptionState {
    pub(super) fn notice(provider: NativeProvider) -> CompatibilityNotice {
        match provider {
            NativeProvider::Codex => {
                let profile = plakous_openai::CodexSubscriptionIntegrationProfile;
                CompatibilityNotice {
                    provider,
                    profile_id: profile.id(),
                    source_revision: profile.source_revision(),
                    secondary_source_revision: None,
                    notice_revision: profile.notice_revision(),
                    provenance: "pinned_compatibility",
                    reviewed_at: profile.reviewed_at(),
                    expires_at: profile.expires_at(),
                    kill_switch_revision: profile.kill_switch_revision(),
                    killed: profile.is_killed(),
                }
            }
            NativeProvider::Xai => {
                let profile = plakous_xai::XaiSubscriptionIntegrationProfile;
                CompatibilityNotice {
                    provider,
                    profile_id: profile.id(),
                    source_revision: profile.source_revision(),
                    secondary_source_revision: Some(profile.official_model_source_revision()),
                    notice_revision: profile.notice_revision(),
                    provenance: "pinned_compatibility",
                    reviewed_at: profile.reviewed_at(),
                    expires_at: profile.expires_at(),
                    kill_switch_revision: profile.kill_switch_revision(),
                    killed: profile.is_killed(),
                }
            }
            NativeProvider::Claude => {
                let profile = plakous_anthropic::ClaudeSubscriptionIntegrationProfile;
                CompatibilityNotice {
                    provider,
                    profile_id: profile.id(),
                    source_revision: profile.source_revision(),
                    secondary_source_revision: None,
                    notice_revision: profile.notice_revision(),
                    provenance: "pinned_compatibility",
                    reviewed_at: profile.reviewed_at(),
                    expires_at: profile.expires_at(),
                    kill_switch_revision: profile.kill_switch_revision(),
                    killed: profile.is_killed(),
                }
            }
        }
    }
}

#[cfg(all(unix, test))]
use filesystem::read_private_bounded_with_hook;
#[cfg(test)]
use recovery::{
    clear_login_recovery_with_hook, login_epoch_matches, login_recovery_required,
    repin_cleared_login_recovery_with_hook,
};

#[cfg(all(unix, not(test)))]
use filesystem::AtomicWriteStage;
