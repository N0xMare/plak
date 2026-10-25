//! Durable, provider-neutral private credential storage.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use thiserror::Error;

#[cfg(feature = "http-refresh")]
use plakous_http::{
    Failure as HttpFailure, Headers as HttpHeaders, ResponseBody as HttpResponseBody, StatusCode,
    Version,
};

use crate::{
    CredentialFence, CredentialGeneration, CredentialRecord, CredentialSlotId, CredentialStatus,
    SecretBytes, SubscriptionIntegrationProfileId,
};

#[cfg(feature = "http-refresh")]
mod http;
mod lifecycle;
mod record;
mod refresh;

use record::{decode_record, encode_record};

const FORMAT_VERSION: u32 = 2;
const MAX_RECORD_FILE_BYTES: u64 = 384 * 1024;
const MAX_RECORD_NAME_BYTES: usize = 128;
const MAX_LOCK_IDENTITY_RETRIES: usize = 4;

#[derive(Clone, Copy, Eq, PartialEq)]
struct StoreAuthority {
    device: u64,
    inode: u64,
}

/// A non-cloneable lease over one exact Ready credential record.
///
/// The lease owns both the decoded record and a shared cross-process lock.
/// Refresh, replacement, logout, and revoke remain blocked until it is
/// dropped. This type intentionally implements neither `Clone` nor Serde.
pub struct ReadyCredentialLease {
    record: CredentialRecord,
    _lock: platform::StoreLock,
}

impl ReadyCredentialLease {
    /// Borrows the provider-opaque payload while the shared lock is retained.
    #[must_use]
    pub const fn payload(&self) -> &SecretBytes {
        self.record.payload()
    }

    /// Returns a credential-free fence for the leased record.
    #[must_use]
    pub fn fence(&self) -> CredentialFence {
        self.record.fence()
    }
}

impl fmt::Debug for ReadyCredentialLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReadyCredentialLease(<redacted ready record>)")
    }
}

struct RefreshClaimCore {
    authority: StoreAuthority,
    record_name: OsString,
    slot_id: CredentialSlotId,
    record: CredentialRecord,
    previous_generation: CredentialGeneration,
    previous_status: CredentialStatus,
    /// The exclusive execution lock (plus private directory) taken before the
    /// durable Unknown write and retained until the claim is consumed or
    /// dropped. Its release is the cross-process signal that the exchange
    /// finished.
    execution_lock: platform::StoreLock,
    /// Identity of the record lock file when the claim was written. Every
    /// claim-owned write re-acquires the record lock and refuses a replaced
    /// inode, so a swapped lock file cannot open a second mutation lane.
    record_lock_identity: StoreAuthority,
}

/// A non-cloneable capability proving a durable, not-yet-started refresh claim.
///
/// The store has already advanced and durably persisted the old credential as
/// [`CredentialStatus::RefreshOutcomeUnknown`] while owning the exclusive
/// refresh execution lock, which the claim retains. Its payload may be
/// borrowed to construct the concrete refresh request before moving both into
/// the store-owned HTTP transaction, but provider I/O is not yet permitted. It
/// may be restored only while provider I/O is definitely unstarted. Consuming
/// [`Self::start`] irreversibly moves to [`StartedRefreshClaim`]. Dropping
/// either claim intentionally leaves the quarantine in place and releases the
/// execution lock so a peer may reclaim.
pub struct RefreshClaim {
    core: RefreshClaimCore,
}

impl RefreshClaim {
    /// Returns the durable Unknown fence created before provider I/O.
    #[must_use]
    pub fn fence(&self) -> CredentialFence {
        self.core.record.fence()
    }

    /// Returns the pre-claim generation.
    #[must_use]
    pub const fn previous_generation(&self) -> CredentialGeneration {
        self.core.previous_generation
    }

    /// Returns the pre-claim lifecycle status.
    #[must_use]
    pub const fn previous_status(&self) -> CredentialStatus {
        self.core.previous_status
    }

    /// Borrows the old provider payload for local request construction.
    ///
    /// This does not authorize transmission. Callers using the `http-refresh`
    /// bridge must finish constructing the concrete request locally, then move
    /// this exact claim into [`PrivateCredentialStore::execute_claimed_http`],
    /// which owns the adjacent start/execute boundary.
    #[must_use]
    pub const fn payload(&self) -> &SecretBytes {
        self.core.record.payload()
    }

    /// Irreversibly marks provider I/O as permitted and exposes the old payload.
    ///
    /// The returned started claim can complete or remain quarantined, but has
    /// no API for the definitely-not-started restoration path.
    #[must_use]
    pub fn start(self) -> StartedRefreshClaim {
        StartedRefreshClaim { core: self.core }
    }
}

impl fmt::Debug for RefreshClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RefreshClaim(<redacted claimed record>)")
    }
}

/// A non-cloneable, network-usable refresh capability.
///
/// It is created by consuming [`RefreshClaim::start`] and therefore cannot use
/// the definitely-not-started restoration API. Dropping it leaves the durable
/// Unknown quarantine unchanged.
pub struct StartedRefreshClaim {
    core: RefreshClaimCore,
}

impl StartedRefreshClaim {
    /// Returns the durable Unknown fence that binds this claim.
    #[must_use]
    pub fn fence(&self) -> CredentialFence {
        self.core.record.fence()
    }

    /// Returns the pre-claim generation.
    #[must_use]
    pub const fn previous_generation(&self) -> CredentialGeneration {
        self.core.previous_generation
    }

    /// Returns the pre-claim lifecycle status.
    #[must_use]
    pub const fn previous_status(&self) -> CredentialStatus {
        self.core.previous_status
    }

    /// Borrows the exact old provider payload for provider-owned network work.
    #[must_use]
    pub const fn payload(&self) -> &SecretBytes {
        self.core.record.payload()
    }
}

impl fmt::Debug for StartedRefreshClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StartedRefreshClaim(<redacted started claim>)")
    }
}

/// The durable disposition of a failed claimed HTTP exchange.
///
/// Values are observations produced only by [`PrivateCredentialStore`]. A
/// pre-transport rejection may return the unchanged unstarted claim through
/// [`ClaimedHttpError::into_unstarted_claim`]; no disposition or error value is
/// accepted as authority to restore or clear a quarantine.
#[cfg(feature = "http-refresh")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ClaimedHttpDisposition {
    /// Exact store preflight rejected the transaction before transport began.
    ClaimRejected,
    /// The concrete exchange proved it did not start and the old record was restored.
    RestoredNotStarted,
    /// The concrete exchange may have transmitted and the exact Unknown record was verified.
    QuarantinedMayHaveTransmitted,
    /// A definitely-not-started exchange could not restore the exact claimed record.
    RestorationFailed,
    /// An ambiguous exchange could not verify the exact claimed Unknown record.
    QuarantineVerificationFailed,
}

/// A successful response stream inseparably paired with its started claim.
///
/// The private fields and lack of a constructor prevent fabrication or mixing.
/// The response body remains pull-driven while this value retains both the
/// exact claim and its exclusive store execution lock. Only consuming terminal
/// methods on [`PrivateCredentialStore`] may complete, revoke, or verify the
/// quarantine.
///
/// ```compile_fail
/// # use plakous_auth::ClaimedHttpResponse;
/// fn split(response: ClaimedHttpResponse) {
///     let (_response, _claim) = response.into_parts();
/// }
/// ```
#[cfg(feature = "http-refresh")]
pub struct ClaimedHttpResponse {
    status: StatusCode,
    version: Version,
    headers: HttpHeaders,
    body: HttpResponseBody,
    claim: StartedRefreshClaim,
    clean_eof: bool,
    body_failed: bool,
    transport_failure: Option<HttpFailure>,
}

/// A redacted failure from a store-owned claimed HTTP transaction.
///
/// This type has no public constructor. Its transport and store errors are
/// observations only; no API accepts them as restoration or quarantine proof.
/// When preflight rejected the transaction before the client ran, it may own
/// the unchanged unstarted claim for exact recovery.
#[cfg(feature = "http-refresh")]
pub struct ClaimedHttpError {
    disposition: ClaimedHttpDisposition,
    transport: Option<HttpFailure>,
    store: Option<PrivateStoreError>,
    unstarted_claim: Option<RefreshClaim>,
}

/// An explicit-path store for exactly one profile/slot credential record.
///
/// Mutations obtain the same per-record cross-process exclusive lock and a
/// private-directory writer lock. The directory lock intentionally serializes
/// writes to sibling records so replacing a held record lock cannot open a
/// second mutation lane. A refresh claim owns a separate exclusive execution
/// lock plus the directory for its whole lifetime, so it excludes writers and
/// sibling claims while shared readers and Ready leases, which hold only the
/// record lock, never wait behind a provider exchange. Both reload and
/// validate under their locks. The store performs no home-directory,
/// environment-variable, or provider discovery.
pub struct PrivateCredentialStore {
    path: PathBuf,
    record_name: OsString,
    lock_name: OsString,
    execution_lock_name: OsString,
    profile_id: SubscriptionIntegrationProfileId,
    slot_id: CredentialSlotId,
    authority: StoreAuthority,
    directory: platform::DirectoryHandle,
}

impl PrivateCredentialStore {
    /// Opens a restartable selected-account slot at an explicit record path.
    ///
    /// The slot is a local selector; each record separately stores the
    /// actual provider-derived [`AccountBinding`](crate::AccountBinding). No
    /// account value is inferred from the slot and no ambient path discovery
    /// occurs.
    ///
    /// The record may be absent, but its parent directory must already exist,
    /// be owned by the effective user, have mode `0700`, and contain no symlink
    /// component. The record and lock file, when present, must be owned regular
    /// files with mode `0600` and exactly one hard link.
    ///
    /// # Errors
    ///
    /// Fails closed when the platform cannot provide the required semantics or
    /// when the explicit path or private directory is unsafe.
    pub fn open_slot(
        path: impl Into<PathBuf>,
        profile_id: SubscriptionIntegrationProfileId,
        slot_id: CredentialSlotId,
    ) -> Result<Self, PrivateStoreError> {
        let explicit_path = path.into();
        let (path, record_name, lock_name, execution_lock_name) =
            validate_explicit_path(&explicit_path)?;
        let (directory, authority) = platform::open_private_directory(&path)?;
        Ok(Self {
            path,
            record_name,
            lock_name,
            execution_lock_name,
            profile_id,
            slot_id,
            authority,
            directory,
        })
    }

    /// Returns the normalized explicit record path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the exact profile scope accepted by this handle.
    #[must_use]
    pub const fn profile_id(&self) -> &SubscriptionIntegrationProfileId {
        &self.profile_id
    }

    /// Returns the selected-account slot this handle is bound to.
    ///
    /// The actual account is stored in each credential record and may change
    /// only through [`Self::replace_selected_login`].
    #[must_use]
    pub const fn slot_id(&self) -> &CredentialSlotId {
        &self.slot_id
    }

    /// Returns the lock path for security inspection and operational cleanup.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.path.with_file_name(&self.lock_name)
    }

    /// Whether a refresh claim, in this or another process, currently owns
    /// the execution lock. Never waits; creates and writes nothing.
    fn refresh_claim_in_flight(&self) -> Result<bool, PrivateStoreError> {
        platform::execution_lock_held(self)
    }

    /// Seeds an absent store with a record in any lifecycle state.
    #[cfg(test)]
    fn initialize(&self, record: &CredentialRecord) -> Result<(), PrivateStoreError> {
        self.require_scope(record)?;
        self.with_lock(|store| {
            if platform::load_record(store)?.is_some() {
                return Err(PrivateStoreError::AlreadyInitialized);
            }
            platform::write_record(store, record)
        })
    }

    fn begin_refresh_with_options(
        &self,
        expected: &CredentialFence,
        allow_unknown: bool,
    ) -> Result<RefreshClaim, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        self.claim_under_execution_lock(|current| {
            if current.fence() != *expected {
                return Err(PrivateStoreError::FenceMismatch);
            }
            Ok(allow_unknown)
        })
    }

    /// Takes the execution lock, then claims the loaded record under a brief
    /// exclusive record lock.
    ///
    /// The execution lock is acquired without waiting and before anything is
    /// written: a store owned by another writer or an in-flight claim yields
    /// [`PrivateStoreError::StoreBusy`] with the durable record untouched.
    /// `admit` validates the loaded record and returns whether a durable
    /// Unknown record may be reclaimed.
    fn claim_under_execution_lock(
        &self,
        admit: impl FnOnce(&CredentialRecord) -> Result<bool, PrivateStoreError>,
    ) -> Result<RefreshClaim, PrivateStoreError> {
        let execution_lock =
            platform::try_lock_execution(self)?.ok_or(PrivateStoreError::StoreBusy)?;
        platform::verify_lock_authority(self, &execution_lock)?;
        let record_lock =
            platform::lock_record_for_claim(self, false)?.ok_or(PrivateStoreError::LockFailed)?;
        platform::verify_lock_authority(self, &record_lock)?;
        let record_lock_identity = record_lock.file_identity()?;
        let current = platform::load_record(self)?.ok_or(PrivateStoreError::RecordMissing)?;
        let allow_unknown = admit(&current)?;
        let claim =
            self.claim_loaded_record(current, allow_unknown, execution_lock, record_lock_identity)?;
        platform::verify_lock_authority(self, &record_lock)?;
        platform::verify_lock_authority(self, &claim.core.execution_lock)?;
        drop(record_lock);
        Ok(claim)
    }

    /// Acquires the record lock for a claim-owned write and refuses a record
    /// lock file whose identity changed since the claim was written.
    fn claim_record_lock(
        &self,
        claim: &RefreshClaimCore,
        nonblocking: bool,
    ) -> Result<Option<platform::StoreLock>, PrivateStoreError> {
        platform::verify_lock_authority(self, &claim.execution_lock)?;
        let Some(record_lock) = platform::lock_record_for_claim(self, nonblocking)? else {
            return Ok(None);
        };
        platform::verify_lock_authority(self, &record_lock)?;
        if record_lock.file_identity()? != claim.record_lock_identity {
            return Err(PrivateStoreError::UnsafeLock);
        }
        Ok(Some(record_lock))
    }

    /// Rewrites the exact Unknown `expected` record as Ready at the next
    /// generation; the caller holds the exclusive writer lock.
    fn restore_unknown_record(
        &self,
        expected: &CredentialFence,
    ) -> Result<CredentialFence, PrivateStoreError> {
        let mut current = platform::load_record(self)?.ok_or(PrivateStoreError::RecordMissing)?;
        if current.fence() != *expected {
            return Err(PrivateStoreError::FenceMismatch);
        }
        if current.status() != CredentialStatus::RefreshOutcomeUnknown {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        let next_generation = current
            .generation()
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        current.set_generation(next_generation);
        current.set_status(CredentialStatus::Ready);
        platform::write_record(self, &current)?;
        Ok(current.fence())
    }

    fn require_scope(&self, record: &CredentialRecord) -> Result<(), PrivateStoreError> {
        if record.profile_id() != &self.profile_id {
            return Err(PrivateStoreError::ForeignProfile);
        }
        Ok(())
    }

    fn require_fence_scope(&self, fence: &CredentialFence) -> Result<(), PrivateStoreError> {
        if fence.profile_id() != &self.profile_id {
            return Err(PrivateStoreError::ForeignProfile);
        }
        Ok(())
    }

    fn claim_loaded_record(
        &self,
        mut current: CredentialRecord,
        allow_unknown: bool,
        execution_lock: platform::StoreLock,
        record_lock_identity: StoreAuthority,
    ) -> Result<RefreshClaim, PrivateStoreError> {
        match current.status() {
            CredentialStatus::Revoked => return Err(PrivateStoreError::CredentialRevoked),
            CredentialStatus::RefreshOutcomeUnknown if !allow_unknown => {
                return Err(PrivateStoreError::CredentialQuarantined);
            }
            CredentialStatus::Ready
            | CredentialStatus::RefreshRequired
            | CredentialStatus::RefreshOutcomeUnknown => {}
        }
        let previous_generation = current.generation();
        let previous_status = current.status();
        let claim_generation = previous_generation
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        claim_generation
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        current.set_generation(claim_generation);
        current.set_status(CredentialStatus::RefreshOutcomeUnknown);
        platform::write_record(self, &current)?;
        Ok(RefreshClaim {
            core: RefreshClaimCore {
                authority: self.authority,
                record_name: self.record_name.clone(),
                slot_id: self.slot_id.clone(),
                record: current,
                previous_generation,
                previous_status,
                execution_lock,
                record_lock_identity,
            },
        })
    }

    /// Verifies the exact claimed Unknown record under the claim's own
    /// execution lock.
    ///
    /// No record lock is needed for this read: the claim owns the private
    /// directory, so no writer can replace the record concurrently.
    fn verify_claimed_http_quarantine_under_lock(
        &self,
        claim: &RefreshClaimCore,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_claim_scope(claim)?;
        platform::verify_lock_authority(self, &claim.execution_lock)?;
        let current = platform::load_record(self)?.ok_or(PrivateStoreError::RecordMissing)?;
        Self::verify_claim_record(claim, &current)?;
        platform::verify_lock_authority(self, &claim.execution_lock)?;
        Ok(current.fence())
    }

    /// Restores the pre-claim status at the next generation, waiting for
    /// shared readers and leases to drain.
    fn restore_claimed_http_not_started_under_lock(
        &self,
        claim: &mut RefreshClaim,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_claim_scope(&claim.core)?;
        let record_lock = self
            .claim_record_lock(&claim.core, false)?
            .ok_or(PrivateStoreError::LockFailed)?;
        let restored = self.restore_claim_record(claim)?;
        platform::verify_lock_authority(self, &record_lock)?;
        Ok(restored)
    }

    /// Writes the restored record; the caller holds the record lock.
    fn restore_claim_record(
        &self,
        claim: &mut RefreshClaim,
    ) -> Result<CredentialFence, PrivateStoreError> {
        let restored_generation = claim
            .core
            .record
            .generation()
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        self.verify_claimed_http_quarantine_under_lock(&claim.core)?;
        claim.core.record.set_generation(restored_generation);
        claim.core.record.set_status(claim.core.previous_status);
        platform::write_record(self, &claim.core.record)?;
        platform::verify_lock_authority(self, &claim.core.execution_lock)?;
        Ok(claim.core.record.fence())
    }

    fn require_claim_scope(&self, claim: &RefreshClaimCore) -> Result<(), PrivateStoreError> {
        if claim.authority != self.authority
            || claim.record_name != self.record_name
            || claim.slot_id != self.slot_id
        {
            return Err(PrivateStoreError::RefreshClaimMismatch);
        }
        self.require_scope(&claim.record)
    }

    fn verify_claim_record(
        claim: &RefreshClaimCore,
        current: &CredentialRecord,
    ) -> Result<(), PrivateStoreError> {
        if current.fence() != claim.record.fence() {
            return Err(PrivateStoreError::RefreshClaimMismatch);
        }
        let claim_digest = Sha256::digest(claim.record.payload().expose_secret());
        let current_digest = Sha256::digest(current.payload().expose_secret());
        if !bool::from(claim_digest.ct_eq(&current_digest)) {
            return Err(PrivateStoreError::RefreshClaimMismatch);
        }
        Ok(())
    }

    fn ready_lease_under_lock(
        &self,
        expected: &CredentialFence,
        lock: platform::StoreLock,
    ) -> Result<ReadyCredentialLease, PrivateStoreError> {
        let record = platform::load_record(self)?.ok_or(PrivateStoreError::RecordMissing)?;
        if record.fence() != *expected {
            return Err(PrivateStoreError::FenceMismatch);
        }
        if record.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        platform::verify_lock_authority(self, &lock)?;
        Ok(ReadyCredentialLease {
            record,
            _lock: lock,
        })
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce(&Self) -> Result<T, PrivateStoreError>,
    ) -> Result<T, PrivateStoreError> {
        let lock = platform::lock(self)?;
        platform::verify_lock_authority(self, &lock)?;
        let result = operation(self);
        platform::verify_lock_authority(self, &lock)?;
        result
    }

    fn with_shared_lock<T>(
        &self,
        operation: impl FnOnce(&Self) -> Result<T, PrivateStoreError>,
    ) -> Result<T, PrivateStoreError> {
        let lock = platform::lock_shared(self, false)?.ok_or(PrivateStoreError::LockFailed)?;
        platform::verify_lock_authority(self, &lock)?;
        let result = operation(self);
        platform::verify_lock_authority(self, &lock)?;
        result
    }

    /// Runs a claim-owned record write: exclusive on the record lock file only,
    /// because the claim's execution lock already owns the private directory.
    fn with_claim_record_lock<T>(
        &self,
        claim: &RefreshClaimCore,
        operation: impl FnOnce(&Self) -> Result<T, PrivateStoreError>,
    ) -> Result<T, PrivateStoreError> {
        let lock = self
            .claim_record_lock(claim, false)?
            .ok_or(PrivateStoreError::LockFailed)?;
        let result = operation(self);
        platform::verify_lock_authority(self, &lock)?;
        platform::verify_lock_authority(self, &claim.execution_lock)?;
        result
    }
}

impl fmt::Debug for PrivateCredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateCredentialStore")
            .field("path", &self.path)
            .field("scope", &"<redacted opaque scope>")
            .finish_non_exhaustive()
    }
}

/// Fail-closed private credential store error.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum PrivateStoreError {
    /// The target lacks the required Unix filesystem guarantees.
    #[error("private credential storage is unsupported on this platform")]
    UnsupportedPlatform,
    /// The explicit record path is not an absolute bounded file path.
    #[error("invalid explicit credential store path")]
    InvalidPath,
    /// The private directory is missing, replaced, linked, or not a directory.
    #[error("unsafe private credential directory")]
    UnsafeDirectory,
    /// A directory, lock, or record is not owned by the effective user.
    #[error("unsafe credential store ownership")]
    UnsafeOwnership,
    /// A directory, lock, or record has permissions broader than allowed.
    #[error("unsafe credential store permissions")]
    UnsafePermissions,
    /// The record is a symlink, hard link, or non-regular filesystem object.
    #[error("unsafe credential record")]
    UnsafeRecord,
    /// The lock is a symlink, hard link, or non-regular filesystem object.
    #[error("unsafe credential lock file")]
    UnsafeLock,
    /// The advisory cross-process lock could not be established.
    #[error("credential store locking failed")]
    LockFailed,
    /// Another writer or refresh claim currently owns the store; nothing was
    /// written. A local contention fact, never a provider outcome.
    #[error("credential store is busy")]
    StoreBusy,
    /// A bounded filesystem operation failed.
    #[error("credential store I/O failed")]
    Io,
    /// Secure randomness for an exclusive temporary name was unavailable.
    #[error("secure temporary-name randomness is unavailable")]
    RandomnessUnavailable,
    /// No current record exists.
    #[error("credential record is missing")]
    RecordMissing,
    /// Initialization was attempted over an existing record.
    #[error("credential record is already initialized")]
    AlreadyInitialized,
    /// The on-disk document is syntactically or structurally malformed.
    #[error("malformed credential record")]
    MalformedRecord,
    /// The on-disk format version is older than this build reads.
    #[error("unsupported credential record version")]
    UnsupportedVersion,
    /// A newer build wrote the record (a newer envelope format or a newer
    /// compatibility profile). This build reads nothing else from it and
    /// writes nothing over it except an explicit local retirement.
    #[error("credential record was written by a newer build")]
    WrittenByNewerBuild,
    /// The encoded record exceeds its fixed input bound.
    #[error("credential record exceeds the storage bound")]
    RecordTooLarge,
    /// The decoded provider payload exceeds its fixed bound.
    #[error("credential payload exceeds the storage bound")]
    PayloadTooLarge,
    /// The stored or proposed profile does not match this handle.
    #[error("foreign credential profile")]
    ForeignProfile,
    /// A generic replacement tried to change the stored actual account.
    #[error("foreign credential account")]
    ForeignAccount,
    /// The stored selected-account slot does not match this handle.
    #[error("foreign credential slot")]
    ForeignSlot,
    /// The exact credential fence no longer matches stored state.
    #[error("credential fence mismatch")]
    FenceMismatch,
    /// A Ready-only operation encountered another lifecycle state.
    #[error("credential is not ready")]
    CredentialNotReady,
    /// A refresh claim is stale, altered, or belongs to another store.
    #[error("refresh claim mismatch")]
    RefreshClaimMismatch,
    /// The current generation differs from the caller's CAS expectation.
    #[error("stale credential writer")]
    StaleWriter,
    /// The replacement generation did not strictly advance.
    #[error("credential generation rollback rejected")]
    GenerationRollback,
    /// A selected login replacement did not advance by exactly one.
    #[error("credential generation must advance by exactly one")]
    GenerationNotSequential,
    /// The generation cannot advance beyond `u64::MAX`.
    #[error("credential generation exhausted")]
    GenerationExhausted,
    /// Refresh was attempted for a revoked record.
    #[error("credential is revoked")]
    CredentialRevoked,
    /// An unresolved refresh claim cannot be claimed again.
    #[error("credential refresh outcome is quarantined")]
    CredentialQuarantined,
    /// A proposed record violates credential lifecycle invariants.
    #[error("invalid credential record")]
    InvalidRecord,
    /// A claimed HTTP response body did not reach clean end-of-stream.
    #[cfg(feature = "http-refresh")]
    #[error("claimed HTTP response body is not complete")]
    HttpResponseBodyNotComplete,
}

type ExplicitPathParts = (PathBuf, OsString, OsString, OsString);

fn validate_explicit_path(path: &Path) -> Result<ExplicitPathParts, PrivateStoreError> {
    if !path.is_absolute() {
        return Err(PrivateStoreError::InvalidPath);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => return Err(PrivateStoreError::InvalidPath),
        }
    }
    let record = normalized
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(PrivateStoreError::InvalidPath)?;
    if record.is_empty()
        || record.len() > MAX_RECORD_NAME_BYTES
        || !record
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
    {
        return Err(PrivateStoreError::InvalidPath);
    }
    let record_name = OsString::from(record);
    let lock_name = OsString::from(format!(".{record}.lock"));
    let execution_lock_name = OsString::from(format!(".{record}.refresh.lock"));
    Ok((normalized, record_name, lock_name, execution_lock_name))
}

#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd"
))]
#[path = "private_store/platform/unix.rs"]
mod platform;

#[cfg(not(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
#[path = "private_store/platform/unsupported.rs"]
mod platform;

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
use crate::AccountBinding;
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
use std::fs::{self, OpenOptions};
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
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
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
use std::sync::mpsc;
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
use std::thread;
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
use std::time::Duration;
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
use tempfile::TempDir;

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
const LOCK_HELPER_PATH: &str = "PLAKOUS_AUTH_TEST_LOCK_PATH";
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
const LOCK_HELPER_READY: &str = "PLAKOUS_AUTH_TEST_LOCK_READY";
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
const LOCK_HELPER_RELEASE: &str = "PLAKOUS_AUTH_TEST_LOCK_RELEASE";
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
const LOCK_HELPER_SHARED_LEASE: &str = "PLAKOUS_AUTH_TEST_SHARED_LEASE";

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
struct Fixture {
    directory: TempDir,
    path: PathBuf,
    profile: SubscriptionIntegrationProfileId,
    slot: CredentialSlotId,
    account: AccountBinding,
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
impl Fixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            path: directory.path().join("credentials.json"),
            directory,
            profile: SubscriptionIntegrationProfileId::new("profile")?,
            slot: CredentialSlotId::new("primary")?,
            account: AccountBinding::new("opaque-account")?,
        })
    }

    fn store(&self) -> Result<PrivateCredentialStore, PrivateStoreError> {
        PrivateCredentialStore::open_slot(&self.path, self.profile.clone(), self.slot.clone())
    }

    fn record(
        &self,
        generation: u64,
        status: CredentialStatus,
        payload: &[u8],
    ) -> Result<CredentialRecord, Box<dyn std::error::Error>> {
        Ok(CredentialRecord::new(
            self.profile.clone(),
            self.account.clone(),
            CredentialGeneration::new(generation)?,
            status,
            SecretBytes::new(payload.to_vec()),
        )?)
    }
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
struct RotatedDirectory {
    original: PathBuf,
    displaced: PathBuf,
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
impl RotatedDirectory {
    fn replace(original: &Path) -> Result<Self, std::io::Error> {
        let displaced = original.with_extension("authority-displaced");
        fs::rename(original, &displaced)?;
        if let Err(error) = fs::create_dir(original) {
            let _ = fs::rename(&displaced, original);
            return Err(error);
        }
        if let Err(error) = fs::set_permissions(original, fs::Permissions::from_mode(0o700)) {
            let _ = fs::remove_dir(original);
            let _ = fs::rename(&displaced, original);
            return Err(error);
        }
        Ok(Self {
            original: original.to_path_buf(),
            displaced,
        })
    }
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
impl Drop for RotatedDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.original);
        let _ = fs::rename(&self.displaced, &self.original);
    }
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
fn load_raw_for_test(
    store: &PrivateCredentialStore,
) -> Result<Option<CredentialRecord>, PrivateStoreError> {
    store.with_lock(platform::load_record)
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
fn run_after_blocked_directory_rotation<T: Send + 'static>(
    fixture: &Fixture,
    operation: impl FnOnce(PrivateCredentialStore) -> Result<T, PrivateStoreError> + Send + 'static,
) -> Result<(Result<T, PrivateStoreError>, RotatedDirectory), Box<dyn std::error::Error>> {
    let blocker_store = fixture.store()?;
    let blocker = platform::lock(&blocker_store)?;
    let contender = fixture.store()?;
    let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
    let (resume_sender, resume_receiver) = mpsc::sync_channel(0);
    let worker = thread::spawn(move || {
        let mut resume_receiver = Some(resume_receiver);
        platform::with_test_before_flock_observer(
            move |attempt| {
                assert_eq!(attempt, 0);
                ready_sender
                    .send(())
                    .map_err(|_| PrivateStoreError::LockFailed)?;
                let receiver = resume_receiver
                    .take()
                    .ok_or(PrivateStoreError::LockFailed)?;
                receiver.recv().map_err(|_| PrivateStoreError::LockFailed)
            },
            || operation(contender),
        )
    });

    ready_receiver.recv()?;
    let rotation = RotatedDirectory::replace(fixture.directory.path())?;
    resume_sender.send(())?;
    thread::sleep(Duration::from_millis(50));
    assert!(
        !worker.is_finished(),
        "contender did not block on the held flock"
    );
    drop(blocker);
    let result = worker.join().map_err(|_| "blocked contender panicked")?;
    Ok((result, rotation))
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
fn create_private_file(path: &Path) -> Result<(), PrivateStoreError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| PrivateStoreError::Io)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|_| PrivateStoreError::Io)
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
fn wait_for_file(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..400 {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err("timed out waiting for helper process".into())
}
