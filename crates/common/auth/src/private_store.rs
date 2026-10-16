//! Durable, provider-neutral private credential storage.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use zeroize::{Zeroize as _, Zeroizing};

#[cfg(feature = "http-refresh")]
use plakous_http::{
    Bytes as HttpBytes, Client as HttpClient, ExchangeControl, Failure as HttpFailure,
    Headers as HttpHeaders, Request as HttpRequest, ResponseBody as HttpResponseBody, StatusCode,
    TransmissionFact, Version,
};

use crate::{
    AccountBinding, CredentialFence, CredentialGeneration, CredentialRecord, CredentialSlotId,
    CredentialStatus, CredentialValueError, RecordAge, SecretBytes,
    SubscriptionIntegrationProfileId,
};

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

#[cfg(feature = "http-refresh")]
impl ClaimedHttpResponse {
    /// Returns the wire status without interpreting it.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the negotiated HTTP version.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// Returns bounded, redaction-marked response headers.
    #[must_use]
    pub const fn headers(&self) -> &HttpHeaders {
        &self.headers
    }

    /// Returns the credential-free Unknown fence paired with this response.
    #[must_use]
    pub fn refresh_fence(&self) -> CredentialFence {
        self.claim.fence()
    }

    /// Returns whether the caller has pulled the response body through clean EOF.
    #[must_use]
    pub const fn body_completed_cleanly(&self) -> bool {
        self.clean_eof
    }

    /// Returns the content-free cause of the last failed body pull, for telemetry.
    #[must_use]
    pub const fn transport_failure(&self) -> Option<HttpFailure> {
        self.transport_failure
    }

    /// Pulls one bounded body chunk while retaining the claim and store lock.
    ///
    /// `None` marks clean EOF only when [`Self::body_completed_cleanly`] becomes
    /// true. After a failed or cancelled pull, later calls also return `None`
    /// but completion remains permanently disabled. A body failure leaves the
    /// durable credential Unknown and prevents
    /// [`PrivateCredentialStore::complete_claimed_http`].
    pub async fn next_chunk(&mut self) -> Option<Result<HttpBytes, HttpFailure>> {
        if self.clean_eof || self.body_failed {
            return None;
        }
        self.body_failed = true;
        match self.body.next_chunk().await {
            None => {
                self.clean_eof = true;
                self.body_failed = false;
                None
            }
            Some(Err(error)) => {
                self.transport_failure = Some(error);
                Some(Err(error))
            }
            Some(Ok(chunk)) => {
                self.body_failed = false;
                Some(Ok(chunk))
            }
        }
    }
}

#[cfg(feature = "http-refresh")]
impl fmt::Debug for ClaimedHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaimedHttpResponse(<redacted response and refresh claim>)")
    }
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

#[cfg(feature = "http-refresh")]
impl ClaimedHttpError {
    /// Returns the durable disposition reached by the transaction.
    #[must_use]
    pub const fn disposition(&self) -> ClaimedHttpDisposition {
        self.disposition
    }

    /// Returns the exact redacted failure from `Client::execute`, if it ran.
    #[must_use]
    pub const fn transport_failure(&self) -> Option<HttpFailure> {
        self.transport
    }

    /// Returns a fail-closed store transition error, when one occurred.
    #[must_use]
    pub const fn store_error(&self) -> Option<PrivateStoreError> {
        self.store
    }

    /// Recovers the exact unstarted claim when execution was rejected before
    /// [`HttpClient::execute`] was called.
    ///
    /// This is `None` after any transport attempt. The recovered capability is
    /// suitable only for retry or the existing exact
    /// [`PrivateCredentialStore::restore_refresh_definitely_not_started`]
    /// transition; it is never accepted as proof that a transport attempt did
    /// not start.
    #[must_use]
    pub fn take_unstarted_claim(&mut self) -> Option<RefreshClaim> {
        self.unstarted_claim.take()
    }

    /// Consumes the error and recovers the unstarted claim, when present.
    #[must_use]
    pub fn into_unstarted_claim(mut self) -> Option<RefreshClaim> {
        self.take_unstarted_claim()
    }

    /// Records the durable result of an orchestrator restore attempt.
    pub(crate) fn set_disposition(&mut self, disposition: ClaimedHttpDisposition) {
        self.disposition = disposition;
    }
}

#[cfg(feature = "http-refresh")]
impl fmt::Debug for ClaimedHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaimedHttpError(<redacted claimed HTTP failure>)")
    }
}

#[cfg(feature = "http-refresh")]
impl fmt::Display for ClaimedHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("claimed HTTP refresh transaction failed")
    }
}

#[cfg(feature = "http-refresh")]
impl std::error::Error for ClaimedHttpError {}

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
    /// actual provider-derived [`AccountBinding`]. No account value is inferred
    /// from the slot and no ambient path discovery occurs.
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

    /// Loads only the credential-free fence under the shared record lock.
    ///
    /// This method never returns provider payload bytes. A Ready payload is
    /// available only through [`Self::acquire_ready`], whose returned lease
    /// retains the shared cross-process lock. A read waits only for a
    /// writer's brief record replacement; it never waits behind a refresh
    /// exchange, whose claim owns the execution lock rather than the record
    /// lock, so the durable Unknown fence of an in-flight refresh is visible.
    ///
    /// # Errors
    ///
    /// Rejects unsafe filesystem objects, malformed or unknown formats,
    /// oversized data, and foreign profile/slot records.
    pub fn load(&self) -> Result<Option<CredentialFence>, PrivateStoreError> {
        self.with_shared_lock(
            |store| Ok(platform::load_record(store)?.map(|record| record.fence())),
        )
    }

    /// Reports whether a durable [`CredentialStatus::RefreshOutcomeUnknown`]
    /// `unknown` fence is a settled outcome rather than a refresh claim still
    /// in flight in this or another process.
    ///
    /// A claim writes its outcome before it releases the execution lock, so a
    /// free lock followed by an unchanged re-read is settled. A held lock, a
    /// changed record, or writer contention is unsettled: callers report
    /// retryable contention, never a quarantine that demands a new login. The
    /// probe never waits and never creates or writes a file.
    ///
    /// # Errors
    ///
    /// Rejects unsafe storage objects and lock failures.
    pub fn refresh_outcome_settled(
        &self,
        unknown: &CredentialFence,
    ) -> Result<bool, PrivateStoreError> {
        if self.refresh_claim_in_flight()? {
            return Ok(false);
        }
        match self.try_load() {
            Ok(current) => Ok(current.as_ref() == Some(unknown)),
            Err(PrivateStoreError::StoreBusy) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Whether a refresh claim, in this or another process, currently owns
    /// the execution lock. Never waits; creates and writes nothing.
    fn refresh_claim_in_flight(&self) -> Result<bool, PrivateStoreError> {
        platform::execution_lock_held(self)
    }

    /// Attempts to load the credential-free fence without waiting for another
    /// process that currently owns mutation authority.
    ///
    /// # Errors
    ///
    /// Returns [`PrivateStoreError::StoreBusy`] when the store lock is
    /// contended and otherwise preserves [`Self::load`]'s validation errors.
    pub fn try_load(&self) -> Result<Option<CredentialFence>, PrivateStoreError> {
        let lock = platform::lock_shared(self, true)?.ok_or(PrivateStoreError::StoreBusy)?;
        platform::verify_lock_authority(self, &lock)?;
        let result = platform::load_record(self).map(|record| record.map(|value| value.fence()));
        platform::verify_lock_authority(self, &lock)?;
        result
    }

    /// Loads only the current credential lifecycle status under lock.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::load`].
    pub fn status(&self) -> Result<Option<CredentialStatus>, PrivateStoreError> {
        self.load().map(|fence| fence.map(|value| value.status()))
    }

    /// Reads the current payload in any lifecycle state under the shared
    /// lock, without a lease, claim or write, so a provider can tell its own
    /// payload format version apart before any operation that writes.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::load`].
    pub fn inspect_payload<T>(
        &self,
        inspect: impl FnOnce(&SecretBytes) -> T,
    ) -> Result<Option<T>, PrivateStoreError> {
        self.with_shared_lock(|store| {
            Ok(platform::load_record(store)?.map(|record| inspect(record.payload())))
        })
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

    /// Initializes an absent selected-account slot with its actual account.
    ///
    /// # Errors
    ///
    /// Rejects foreign profiles, non-Ready records, existing records, or
    /// unsafe storage.
    pub fn initialize_selected(&self, record: &CredentialRecord) -> Result<(), PrivateStoreError> {
        self.require_scope(record)?;
        if record.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        self.with_lock(|store| {
            if platform::load_record(store)?.is_some() {
                return Err(PrivateStoreError::AlreadyInitialized);
            }
            platform::write_record(store, record)
        })
    }

    /// Acquires a blocking shared lease for one exact Ready fence.
    ///
    /// The record is reloaded and its profile, account, generation, and Ready
    /// status are checked while the shared cross-process lock is held. The
    /// returned lease retains that lock until drop.
    ///
    /// # Errors
    ///
    /// Rejects a stale/foreign fence, a non-Ready record, missing data, or
    /// unsafe storage.
    pub fn acquire_ready(
        &self,
        expected: &CredentialFence,
    ) -> Result<ReadyCredentialLease, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        if expected.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }

        let lock = platform::lock_shared(self, false)?.ok_or(PrivateStoreError::LockFailed)?;
        self.ready_lease_under_lock(expected, lock)
    }

    /// Attempts to acquire a shared Ready lease without waiting.
    ///
    /// Returns `Ok(None)` only when an exclusive holder currently blocks the
    /// shared lock. Any acquired record is fully reloaded and fence-validated.
    ///
    /// # Errors
    ///
    /// Returns the same validation and storage errors as [`Self::acquire_ready`].
    pub fn try_acquire_ready(
        &self,
        expected: &CredentialFence,
    ) -> Result<Option<ReadyCredentialLease>, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        if expected.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }

        let Some(lock) = platform::lock_shared(self, true)? else {
            return Ok(None);
        };
        self.ready_lease_under_lock(expected, lock).map(Some)
    }

    /// Atomically replaces the record using monotonic generation CAS.
    ///
    /// The current record is reloaded under the cross-process lock. Its
    /// generation must equal `expected_generation`, and the replacement
    /// generation must be strictly greater. Generic CAS may preserve an
    /// Unknown quarantine but can never clear it; clearing requires a
    /// consuming refresh restoration/completion or selected-login operation.
    ///
    /// # Errors
    ///
    /// Rejects stale writers, rollback/equal generations, missing records,
    /// foreign scope, and unsafe storage.
    pub fn replace(
        &self,
        expected_generation: CredentialGeneration,
        replacement: &CredentialRecord,
    ) -> Result<(), PrivateStoreError> {
        self.require_scope(replacement)?;
        self.with_lock(move |store| {
            let current = platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            if current.generation() != expected_generation {
                return Err(PrivateStoreError::StaleWriter);
            }
            if replacement.generation() <= current.generation() {
                return Err(PrivateStoreError::GenerationRollback);
            }
            if current.status() == CredentialStatus::RefreshOutcomeUnknown
                && replacement.status() != CredentialStatus::RefreshOutcomeUnknown
            {
                return Err(PrivateStoreError::CredentialQuarantined);
            }
            if replacement.account_binding() != current.account_binding() {
                return Err(PrivateStoreError::ForeignAccount);
            }
            platform::write_record(store, replacement)
        })
    }

    /// Atomically logs in or switches the actual account in a selected slot.
    ///
    /// The current record must match `expected` exactly. The fully formed,
    /// non-cloneable Ready replacement is consumed as the caller's fresh-login
    /// capability and must advance by exactly one generation. This is the only
    /// slot operation that may change the actual account binding.
    ///
    /// # Errors
    ///
    /// Rejects stale fences, non-Ready records, generation jumps, foreign
    /// profiles, or unsafe storage.
    pub fn replace_selected_login(
        &self,
        expected: &CredentialFence,
        replacement: CredentialRecord,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        self.require_scope(&replacement)?;
        if replacement.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        self.with_lock(move |store| {
            let current = platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            if current.fence() != *expected {
                return Err(PrivateStoreError::FenceMismatch);
            }
            let next = current
                .generation()
                .next()
                .map_err(|_| PrivateStoreError::GenerationExhausted)?;
            if replacement.generation() != next {
                return Err(PrivateStoreError::GenerationNotSequential);
            }
            platform::write_record(store, &replacement)?;
            Ok(replacement.fence())
        })
    }

    /// Begins refresh from one exact credential fence.
    ///
    /// Before returning, this method exclusively reloads the exact fenced
    /// record, advances its generation, and durably writes its old payload as
    /// [`CredentialStatus::RefreshOutcomeUnknown`]. The returned non-cloneable
    /// claim owns but does not expose the same old payload. Provider-owned code
    /// must consume [`RefreshClaim::start`] immediately before network work.
    ///
    /// # Errors
    ///
    /// Rejects a stale/foreign fence, unresolved or revoked state, exhausted
    /// generation space, or unsafe storage.
    pub fn begin_refresh(
        &self,
        expected: &CredentialFence,
    ) -> Result<RefreshClaim, PrivateStoreError> {
        self.begin_refresh_with_options(expected, false)
    }

    /// Begins refresh from an exact fence, allowing a durable
    /// [`CredentialStatus::RefreshOutcomeUnknown`] record to be reclaimed.
    ///
    /// Only dialects that retry the still-stored refresh token after an
    /// unconfirmed exchange may call this. Codex and Claude must use
    /// [`Self::begin_refresh`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::begin_refresh`], except Unknown is claimable when
    /// `expected` matches the loaded record.
    pub fn begin_refresh_reclaiming_unknown(
        &self,
        expected: &CredentialFence,
    ) -> Result<RefreshClaim, PrivateStoreError> {
        self.begin_refresh_with_options(expected, true)
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

    /// Begins refresh using generation CAS.
    ///
    /// The profile is fixed by this store handle; the current record must be
    /// Ready or `RefreshRequired`.
    ///
    /// # Errors
    ///
    /// Returns the same lifecycle and storage errors as [`Self::begin_refresh`].
    pub fn begin_refresh_generation(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<RefreshClaim, PrivateStoreError> {
        self.claim_under_execution_lock(|current| {
            if current.generation() != expected_generation {
                return Err(PrivateStoreError::StaleWriter);
            }
            Ok(false)
        })
    }

    /// Restores a durable [`CredentialStatus::RefreshOutcomeUnknown`] record to
    /// [`CredentialStatus::Ready`] at the next generation with the same payload.
    ///
    /// xAI retries the stored refresh token after an unconfirmed exchange; Codex
    /// and Claude must not call this.
    ///
    /// # Errors
    ///
    /// Rejects a stale/foreign fence, non-Unknown status, exhausted generation
    /// space, or unsafe storage.
    pub fn restore_unknown_to_ready(
        &self,
        expected: &CredentialFence,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        self.with_lock(|store| store.restore_unknown_record(expected))
    }

    /// Attempts [`Self::restore_unknown_to_ready`] once without waiting.
    ///
    /// Returns `Ok(None)` and leaves the record unchanged when a writer, a
    /// reader, a Ready lease, or a refresh claim currently holds the store.
    /// For cancellation paths, which must never block.
    ///
    /// # Errors
    ///
    /// Same as [`Self::restore_unknown_to_ready`].
    pub fn try_restore_unknown_to_ready(
        &self,
        expected: &CredentialFence,
    ) -> Result<Option<CredentialFence>, PrivateStoreError> {
        self.require_fence_scope(expected)?;
        let Some(lock) = platform::try_lock(self)? else {
            return Ok(None);
        };
        platform::verify_lock_authority(self, &lock)?;
        let result = self.restore_unknown_record(expected);
        platform::verify_lock_authority(self, &lock)?;
        result.map(Some)
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

    /// Executes one concrete HTTP exchange under an exact durable refresh claim.
    ///
    /// The claim already owns the exclusive execution lock taken by
    /// `begin_refresh`; this method revalidates the exact scope, explicit-path
    /// authority, Unknown fence, and payload under it. The claim is
    /// irreversibly started immediately before [`HttpClient::execute`].
    /// If that exact call reports [`TransmissionFact::NotStarted`], this method
    /// alone converts the started capability back to its internal unstarted form
    /// and restores the exact old payload and status at the next generation. If
    /// it reports [`TransmissionFact::MayHaveTransmitted`], this method consumes
    /// the claim while verifying that its exact durable Unknown record remains.
    /// Dropping this future after the claim is started leaves that durable
    /// Unknown record unchanged.
    ///
    /// A successful response remains inseparably paired with the started claim
    /// and retained lock while its body is pulled. Any response-body,
    /// validation, or persistence failure leaves Unknown unless a consuming
    /// terminal method explicitly revokes it.
    ///
    /// Local failures that happen while building `request` occur before this
    /// method consumes `claim`; callers retain the separate
    /// [`Self::restore_refresh_definitely_not_started`] path for those failures.
    ///
    /// # Errors
    ///
    /// Returns a redacted error if the claim is foreign, the concrete exchange
    /// fails, or the exact restoration/quarantine verification fails closed.
    #[cfg(feature = "http-refresh")]
    #[allow(
        clippy::result_large_err,
        reason = "the error carries the claim disposition and transport cause back to the refresh owner; it is returned once from an async exchange whose future already holds it"
    )]
    pub async fn execute_claimed_http(
        &self,
        claim: RefreshClaim,
        client: &HttpClient,
        request: HttpRequest,
        control: ExchangeControl,
    ) -> Result<ClaimedHttpResponse, ClaimedHttpError> {
        if let Err(error) = self.verify_claimed_http_quarantine_under_lock(&claim.core) {
            return Err(ClaimedHttpError {
                disposition: ClaimedHttpDisposition::ClaimRejected,
                transport: None,
                store: Some(error),
                unstarted_claim: Some(claim),
            });
        }

        let started = claim.start();
        match client.execute(request, control).await {
            Ok(response) => {
                let status = response.status();
                let version = response.version();
                let headers = response.headers().clone();
                let body = response.into_body();
                Ok(ClaimedHttpResponse {
                    status,
                    version,
                    headers,
                    body,
                    claim: started,
                    clean_eof: false,
                    body_failed: false,
                    transport_failure: None,
                })
            }
            Err(failure) => match failure.transmission() {
                TransmissionFact::NotStarted | TransmissionFact::RequestIncomplete => {
                    let mut unstarted = RefreshClaim { core: started.core };
                    match self.restore_claimed_http_not_started_under_lock(&mut unstarted) {
                        Ok(_) => Err(ClaimedHttpError {
                            disposition: ClaimedHttpDisposition::RestoredNotStarted,
                            transport: Some(failure),
                            store: None,
                            unstarted_claim: None,
                        }),
                        Err(error) => Err(ClaimedHttpError {
                            disposition: ClaimedHttpDisposition::RestorationFailed,
                            transport: Some(failure),
                            store: Some(error),
                            unstarted_claim: None,
                        }),
                    }
                }
                TransmissionFact::MayHaveTransmitted => {
                    match self.verify_claimed_http_quarantine_under_lock(&started.core) {
                        Ok(_) => Err(ClaimedHttpError {
                            disposition: ClaimedHttpDisposition::QuarantinedMayHaveTransmitted,
                            transport: Some(failure),
                            store: None,
                            unstarted_claim: None,
                        }),
                        Err(error) => Err(ClaimedHttpError {
                            disposition: ClaimedHttpDisposition::QuarantineVerificationFailed,
                            transport: Some(failure),
                            store: Some(error),
                            unstarted_claim: None,
                        }),
                    }
                }
            },
        }
    }

    /// Completes a claimed HTTP refresh after its response body reached clean EOF.
    ///
    /// This consumes the inseparable response/claim/lock value and installs one
    /// exact next-generation Ready record while the execution lock remains held.
    /// A response that was not pulled through clean EOF cannot clear Unknown.
    ///
    /// # Errors
    ///
    /// Rejects an incomplete or failed body, a response from another exact
    /// store scope, an invalid replacement, changed path authority or record,
    /// and unsafe storage.
    #[cfg(feature = "http-refresh")]
    pub fn complete_claimed_http(
        &self,
        response: ClaimedHttpResponse,
        replacement: CredentialRecord,
    ) -> Result<CredentialFence, PrivateStoreError> {
        if !response.clean_eof {
            return Err(PrivateStoreError::HttpResponseBodyNotComplete);
        }
        self.require_claim_scope(&response.claim.core)?;
        self.require_scope(&replacement)?;
        if replacement.account_binding() != response.claim.core.record.account_binding() {
            return Err(PrivateStoreError::ForeignAccount);
        }
        if replacement.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        let completion_generation = response
            .claim
            .core
            .record
            .generation()
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        if replacement.generation() != completion_generation {
            return Err(PrivateStoreError::GenerationNotSequential);
        }
        let completed_fence = replacement.fence();
        let result = self.with_claim_record_lock(&response.claim.core, |store| {
            store.verify_claimed_http_quarantine_under_lock(&response.claim.core)?;
            platform::write_record(store, &replacement)
        });
        drop(response);
        drop(replacement);
        result?;
        Ok(completed_fence)
    }

    /// Consumes a claimed HTTP response while preserving and verifying Unknown.
    ///
    /// This is the terminal path for response validation, body, or persistence
    /// failures that must not clear the refresh quarantine.
    ///
    /// # Errors
    ///
    /// Rejects a response from another exact store scope, changed path
    /// authority or claimed record, and unsafe storage.
    #[cfg(feature = "http-refresh")]
    pub fn quarantine_claimed_http(
        &self,
        response: ClaimedHttpResponse,
    ) -> Result<CredentialFence, PrivateStoreError> {
        let fence = self.verify_claimed_http_quarantine_under_lock(&response.claim.core)?;
        drop(response);
        Ok(fence)
    }

    /// Consumes a claimed HTTP response and installs a revoked tombstone.
    ///
    /// Revocation is allowed regardless of response-body completion and
    /// advances exactly once from the retained Unknown claim generation.
    ///
    /// # Errors
    ///
    /// Rejects a response from another exact store scope, changed path
    /// authority or claimed record, exhausted generation space, and unsafe
    /// storage.
    #[cfg(feature = "http-refresh")]
    pub fn logout_claimed_http(
        &self,
        response: ClaimedHttpResponse,
    ) -> Result<CredentialGeneration, PrivateStoreError> {
        self.require_claim_scope(&response.claim.core)?;
        let next = response
            .claim
            .core
            .record
            .generation()
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        let tombstone = CredentialRecord::new(
            self.profile_id.clone(),
            response.claim.core.record.account_binding().clone(),
            next,
            CredentialStatus::Revoked,
            SecretBytes::empty(),
        )
        .map_err(|_| PrivateStoreError::InvalidRecord)?;
        let result = self.with_claim_record_lock(&response.claim.core, |store| {
            store.verify_claimed_http_quarantine_under_lock(&response.claim.core)?;
            platform::write_record(store, &tombstone)
        });
        drop(response);
        result?;
        Ok(next)
    }

    /// Alias for [`Self::logout_claimed_http`] naming local revocation.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::logout_claimed_http`].
    #[cfg(feature = "http-refresh")]
    pub fn revoke_claimed_http(
        &self,
        response: ClaimedHttpResponse,
    ) -> Result<CredentialGeneration, PrivateStoreError> {
        self.logout_claimed_http(response)
    }

    /// Completes refresh by consuming an exact claim and a later Ready record.
    ///
    /// The durable store must still contain the claim's exact Unknown fence and
    /// old payload. Wrong, stale, or cross-store claims cannot clear Unknown.
    ///
    /// # Errors
    ///
    /// Rejects a mismatched claim, non-Ready/foreign replacement, a replacement
    /// that is not exactly one generation after the Unknown claim, or unsafe
    /// storage.
    pub fn complete_refresh(
        &self,
        claim: StartedRefreshClaim,
        replacement: CredentialRecord,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_claim_scope(&claim.core)?;
        self.require_scope(&replacement)?;
        if replacement.account_binding() != claim.core.record.account_binding() {
            return Err(PrivateStoreError::ForeignAccount);
        }
        if replacement.status() != CredentialStatus::Ready {
            return Err(PrivateStoreError::CredentialNotReady);
        }
        let completion_generation = claim
            .core
            .record
            .generation()
            .next()
            .map_err(|_| PrivateStoreError::GenerationExhausted)?;
        if replacement.generation() != completion_generation {
            return Err(PrivateStoreError::GenerationNotSequential);
        }
        let completed_fence = replacement.fence();
        let result = self.with_claim_record_lock(&claim.core, |store| {
            let current = platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            Self::verify_claim_record(&claim.core, &current)?;
            platform::write_record(store, &replacement)
        });
        drop(claim);
        drop(replacement);
        result.map(|()| completed_fence)
    }

    /// Restores the exact old payload/status after proving refresh never began.
    ///
    /// This narrowly named escape hatch consumes the claim and writes the old
    /// payload and pre-claim Ready/RefreshRequired status at a new generation.
    /// It must not be called once provider I/O might have started.
    ///
    /// # Errors
    ///
    /// Rejects stale, wrong, or cross-store claims and unsafe storage.
    pub fn restore_refresh_definitely_not_started(
        &self,
        mut claim: RefreshClaim,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_claim_scope(&claim.core)?;
        let result = self.restore_claimed_http_not_started_under_lock(&mut claim);
        drop(claim);
        result
    }

    /// Attempts one non-blocking restore of an unstarted claim.
    ///
    /// Returns `Ok(None)` when shared readers or Ready leases currently hold
    /// the record lock; the claim is unchanged so the caller can poll. Never
    /// waits. The claim already owns the private directory, so no other
    /// writer can intervene.
    ///
    /// # Errors
    ///
    /// Same scope and record checks as
    /// [`Self::restore_refresh_definitely_not_started`].
    pub fn try_restore_refresh_definitely_not_started(
        &self,
        claim: &mut RefreshClaim,
    ) -> Result<Option<CredentialFence>, PrivateStoreError> {
        self.require_claim_scope(&claim.core)?;
        let Some(record_lock) = self.claim_record_lock(&claim.core, true)? else {
            return Ok(None);
        };
        let restored = self.restore_claim_record(claim)?;
        platform::verify_lock_authority(self, &record_lock)?;
        Ok(Some(restored))
    }

    /// Consumes and verifies a claim while deliberately leaving it quarantined.
    ///
    /// Dropping a claim has the same durable safety property; this explicit
    /// operation additionally proves that the exact Unknown record still exists.
    ///
    /// # Errors
    ///
    /// Rejects stale, wrong, or cross-store claims and unsafe storage.
    pub fn leave_refresh_quarantined(
        &self,
        claim: StartedRefreshClaim,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.require_claim_scope(&claim.core)?;
        let result = self.verify_claimed_http_quarantine_under_lock(&claim.core);
        drop(claim);
        result
    }

    /// Persists refresh intent before provider-owned code sends a request.
    ///
    /// This is an exclusive generation-advancing CAS claim. The record moves
    /// to [`CredentialStatus::RefreshOutcomeUnknown`] at the next generation
    /// before this method returns. Provider-owned code must use that returned
    /// generation for completion or quarantine. A crash therefore leaves the
    /// credential quarantined, and another caller using the old generation
    /// cannot send or clear the claim.
    ///
    /// # Errors
    ///
    /// Rejects stale writers, revoked or missing records, and unsafe storage.
    pub fn mark_refresh_intent(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialFence, PrivateStoreError> {
        let claim = self.begin_refresh_generation(expected_generation)?;
        Ok(claim.fence())
    }

    /// Quarantines a refresh whose network outcome is ambiguous.
    ///
    /// This operation is idempotent for the same generation.
    ///
    /// # Errors
    ///
    /// Rejects stale writers, revoked or missing records, and unsafe storage.
    pub fn quarantine_refresh_outcome(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialFence, PrivateStoreError> {
        self.with_lock(|store| {
            let mut current =
                platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            if current.generation() != expected_generation {
                return Err(PrivateStoreError::StaleWriter);
            }
            if current.status() == CredentialStatus::Revoked {
                return Err(PrivateStoreError::CredentialRevoked);
            }
            if current.status() != CredentialStatus::RefreshOutcomeUnknown {
                current.set_status(CredentialStatus::RefreshOutcomeUnknown);
                platform::write_record(store, &current)?;
            }
            Ok(current.fence())
        })
    }

    /// Logs out by atomically replacing the payload with a revoked tombstone.
    ///
    /// Keeping the next-generation tombstone prevents a stale writer from
    /// resurrecting the credential after logout.
    ///
    /// # Errors
    ///
    /// Rejects stale writers, a missing record, exhausted generation space, or
    /// unsafe storage.
    pub fn logout(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialGeneration, PrivateStoreError> {
        self.with_lock(|store| {
            let current = platform::load_record(store)?.ok_or(PrivateStoreError::RecordMissing)?;
            if current.generation() != expected_generation {
                return Err(PrivateStoreError::StaleWriter);
            }
            let next = current
                .generation()
                .next()
                .map_err(|_| PrivateStoreError::GenerationExhausted)?;
            let tombstone = CredentialRecord::new(
                self.profile_id.clone(),
                current.account_binding().clone(),
                next,
                CredentialStatus::Revoked,
                SecretBytes::empty(),
            )
            .map_err(|_| PrivateStoreError::InvalidRecord)?;
            platform::write_record(store, &tombstone)?;
            Ok(next)
        })
    }

    /// Offline local logout tombstone (no network). Absent → `SignedOut`.
    ///
    /// An explicitly selected slot may contain a prior profile. Local
    /// logout retires that record into an empty current-profile tombstone; it
    /// never grants authority to load, refresh, or reuse the old payload.
    ///
    /// # Errors
    ///
    /// Stale writers, malformed records, or unsafe storage.
    pub fn logout_local(&self) -> Result<crate::OfflineCredentialLifecycle, PrivateStoreError> {
        self.with_lock(|store| {
            let current = match platform::load_record_for_local_retirement(store) {
                Ok(Some(current)) => current,
                Ok(None) => return Ok(crate::OfflineCredentialLifecycle::SignedOut),
                // A newer envelope has no account or generation this build can
                // read to tombstone; retiring it removes it.
                Err(PrivateStoreError::WrittenByNewerBuild) => {
                    platform::remove_record(store)?;
                    return Ok(crate::OfflineCredentialLifecycle::SignedOut);
                }
                Err(error) => return Err(error),
            };
            if current.status() == CredentialStatus::Revoked
                && current.profile_id() == &store.profile_id
            {
                return Ok(crate::OfflineCredentialLifecycle::Revoked);
            }
            let next = current
                .generation()
                .next()
                .map_err(|_| PrivateStoreError::GenerationExhausted)?;
            let tombstone = CredentialRecord::new(
                store.profile_id.clone(),
                current.account_binding().clone(),
                next,
                CredentialStatus::Revoked,
                SecretBytes::empty(),
            )
            .map_err(|_| PrivateStoreError::InvalidRecord)?;
            platform::write_record(store, &tombstone)?;
            Ok(crate::OfflineCredentialLifecycle::Revoked)
        })
    }

    /// Revokes locally using the same stale-writer-safe tombstone as logout.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::logout`].
    pub fn revoke(
        &self,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialGeneration, PrivateStoreError> {
        self.logout(expected_generation)
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

fn encode_record(
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

fn decode_record(
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
mod platform {
    use std::fs::{File, Metadata};
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Path;

    use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};

    use super::*;

    pub(super) type DirectoryHandle = File;

    pub(super) fn open_private_directory(
        path: &Path,
    ) -> Result<(DirectoryHandle, StoreAuthority), PrivateStoreError> {
        let parent = path.parent().ok_or(PrivateStoreError::InvalidPath)?;
        let root = rustix::fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
        let mut directory = File::from(root);
        for component in parent.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    let next = rustix::fs::openat(
                        &directory,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
                    directory = File::from(next);
                    if !directory
                        .metadata()
                        .map_err(|_| PrivateStoreError::UnsafeDirectory)?
                        .file_type()
                        .is_dir()
                    {
                        return Err(PrivateStoreError::UnsafeDirectory);
                    }
                }
                Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                    return Err(PrivateStoreError::InvalidPath);
                }
            }
        }
        let metadata = directory
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
        validate_directory(&metadata)?;
        Ok((
            directory,
            StoreAuthority {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        ))
    }

    /// Exclusive writer lock: the record lock file plus the private directory.
    ///
    /// Every plain mutation takes this. The directory half serializes sibling
    /// records and excludes an in-flight refresh claim, which owns the
    /// directory through its execution lock.
    pub(super) fn lock(store: &PrivateCredentialStore) -> Result<StoreLock, PrivateStoreError> {
        lock_with_mode_observer(store, LockTarget::Record, LockMode::Exclusive, true, |_| {
            Ok(())
        })?
        .ok_or(PrivateStoreError::LockFailed)
    }

    /// Non-blocking form of [`lock`]: `None` when any holder contends.
    pub(super) fn try_lock(
        store: &PrivateCredentialStore,
    ) -> Result<Option<StoreLock>, PrivateStoreError> {
        lock_with_mode_observer(
            store,
            LockTarget::Record,
            LockMode::TryExclusive,
            true,
            |_| Ok(()),
        )
    }

    /// Shared reader lock on the record lock file only.
    ///
    /// Readers and Ready leases never take the directory lock, so a status
    /// read is delayed only by a writer's brief record replacement, never by
    /// a refresh exchange that is waiting on the provider.
    pub(super) fn lock_shared(
        store: &PrivateCredentialStore,
        nonblocking: bool,
    ) -> Result<Option<StoreLock>, PrivateStoreError> {
        let mode = if nonblocking {
            LockMode::TryShared
        } else {
            LockMode::Shared
        };
        lock_with_mode_observer(store, LockTarget::Record, mode, false, |_| Ok(()))
    }

    /// Non-blocking exclusive refresh execution lock plus the private directory.
    ///
    /// A refresh claim owns this from before its durable Unknown write until
    /// completion, restoration, or drop. Holding the directory excludes every
    /// plain writer and every sibling claim; not holding the record lock file
    /// leaves shared readers and Ready leases free.
    pub(super) fn try_lock_execution(
        store: &PrivateCredentialStore,
    ) -> Result<Option<StoreLock>, PrivateStoreError> {
        lock_with_mode_observer(
            store,
            LockTarget::Execution,
            LockMode::TryExclusive,
            true,
            |_| Ok(()),
        )
    }

    /// Probes the execution lock with a non-blocking shared `flock` that is
    /// released before return. A missing lock file means no claim ever ran;
    /// the probe opens only an existing file and creates nothing.
    pub(super) fn execution_lock_held(
        store: &PrivateCredentialStore,
    ) -> Result<bool, PrivateStoreError> {
        verify_explicit_path_authority(store)?;
        let file = match rustix::fs::openat(
            &store.directory,
            LockTarget::Execution.name(store),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => File::from(descriptor),
            Err(rustix::io::Errno::NOENT) => return Ok(false),
            Err(_) => return Err(PrivateStoreError::UnsafeLock),
        };
        validate_file(
            &file.metadata().map_err(|_| PrivateStoreError::UnsafeLock)?,
            true,
        )?;
        match rustix::fs::flock(&file, FlockOperation::NonBlockingLockShared) {
            Ok(()) => Ok(false),
            Err(rustix::io::Errno::AGAIN) => Ok(true),
            Err(_) => Err(PrivateStoreError::LockFailed),
        }
    }

    /// Exclusive record lock file only, for writes made under an owned
    /// execution lock.
    ///
    /// The directory is already held by the claim, so this waits only for
    /// shared readers and Ready leases to drain.
    pub(super) fn lock_record_for_claim(
        store: &PrivateCredentialStore,
        nonblocking: bool,
    ) -> Result<Option<StoreLock>, PrivateStoreError> {
        let mode = if nonblocking {
            LockMode::TryExclusive
        } else {
            LockMode::Exclusive
        };
        lock_with_mode_observer(store, LockTarget::Record, mode, false, |_| Ok(()))
    }

    /// Acquires the private directory (when requested) and then the target
    /// lock file, in that fixed order.
    ///
    /// The order is the global lock order of the store: directory, then
    /// execution lock, then record lock. A refresh claim holds the directory
    /// and later takes the record lock for its writes; taking the directory
    /// before the record lock here keeps plain writers from holding the
    /// record lock while they wait for the claim's directory. Every early
    /// return drops its `File`s, and closing a lock's only descriptor
    /// releases its `flock`.
    fn lock_with_mode_observer(
        store: &PrivateCredentialStore,
        target: LockTarget,
        mode: LockMode,
        lock_directory: bool,
        mut after_open: impl FnMut(usize) -> Result<(), PrivateStoreError>,
    ) -> Result<Option<StoreLock>, PrivateStoreError> {
        validate_directory(
            &store
                .directory
                .metadata()
                .map_err(|_| PrivateStoreError::UnsafeDirectory)?,
        )?;
        let mut last_error = PrivateStoreError::UnsafeLock;
        for attempt in 0..MAX_LOCK_IDENTITY_RETRIES {
            verify_explicit_path_authority(store)?;
            let directory_lock = if lock_directory {
                let (directory, current_authority) = open_private_directory(&store.path)?;
                if current_authority != store.authority {
                    return Err(PrivateStoreError::UnsafeDirectory);
                }
                #[cfg(test)]
                run_test_before_flock_observer(attempt)?;
                match rustix::fs::flock(&directory, mode.operation()) {
                    Ok(()) => {}
                    Err(rustix::io::Errno::AGAIN) if mode.is_nonblocking() => {
                        return Ok(None);
                    }
                    Err(_) => return Err(PrivateStoreError::LockFailed),
                }
                verify_explicit_path_authority(store)?;
                Some(directory)
            } else {
                None
            };
            let (file, created) = open_lock_candidate(store, target)?;
            if created {
                rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
                    .map_err(|_| PrivateStoreError::Io)?;
                file.sync_all().map_err(|_| PrivateStoreError::Io)?;
                store
                    .directory
                    .sync_all()
                    .map_err(|_| PrivateStoreError::Io)?;
            }
            validate_file(
                &file.metadata().map_err(|_| PrivateStoreError::UnsafeLock)?,
                true,
            )?;
            after_open(attempt)?;
            verify_explicit_path_authority(store)?;
            #[cfg(test)]
            if !lock_directory {
                run_test_before_flock_observer(attempt)?;
            }
            match rustix::fs::flock(&file, mode.operation()) {
                Ok(()) => {}
                Err(rustix::io::Errno::AGAIN) if mode.is_nonblocking() => {
                    return Ok(None);
                }
                Err(_) => return Err(PrivateStoreError::LockFailed),
            }
            verify_explicit_path_authority(store)?;
            let Ok(locked_metadata) = file.metadata() else {
                last_error = PrivateStoreError::UnsafeLock;
                continue;
            };
            if let Err(error) = validate_file(&locked_metadata, true) {
                last_error = error;
                continue;
            }
            match open_current_lock(store, target) {
                Ok(current) => {
                    let current_metadata = current
                        .metadata()
                        .map_err(|_| PrivateStoreError::UnsafeLock)?;
                    if same_file_identity(&locked_metadata, &current_metadata) {
                        verify_explicit_path_authority(store)?;
                        return Ok(Some(StoreLock {
                            file,
                            directory: directory_lock,
                            target,
                        }));
                    }
                    last_error = PrivateStoreError::UnsafeLock;
                }
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    pub(super) fn verify_explicit_path_authority(
        store: &PrivateCredentialStore,
    ) -> Result<(), PrivateStoreError> {
        let (current_directory, current_authority) = open_private_directory(&store.path)?;
        if current_authority != store.authority {
            return Err(PrivateStoreError::UnsafeDirectory);
        }
        let retained_metadata = store
            .directory
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
        let current_metadata = current_directory
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
        if !same_file_identity(&retained_metadata, &current_metadata) {
            return Err(PrivateStoreError::UnsafeDirectory);
        }
        Ok(())
    }

    #[cfg(test)]
    type TestLockObserver = Box<dyn FnMut(usize) -> Result<(), PrivateStoreError>>;

    #[cfg(test)]
    thread_local! {
        static TEST_BEFORE_FLOCK_OBSERVER: std::cell::RefCell<Option<TestLockObserver>> =
            const { std::cell::RefCell::new(None) };
    }

    #[cfg(test)]
    fn run_test_before_flock_observer(attempt: usize) -> Result<(), PrivateStoreError> {
        TEST_BEFORE_FLOCK_OBSERVER.with(|observer| {
            if let Some(observer) = observer.borrow_mut().as_mut() {
                observer(attempt)?;
            }
            Ok(())
        })
    }

    #[cfg(test)]
    pub(super) fn with_test_before_flock_observer<T>(
        observer: impl FnMut(usize) -> Result<(), PrivateStoreError> + 'static,
        operation: impl FnOnce() -> T,
    ) -> T {
        struct ClearObserver;

        impl Drop for ClearObserver {
            fn drop(&mut self) {
                TEST_BEFORE_FLOCK_OBSERVER.with(|observer| {
                    observer.borrow_mut().take();
                });
            }
        }

        TEST_BEFORE_FLOCK_OBSERVER.with(|slot| {
            assert!(
                slot.borrow().is_none(),
                "test lock observer already installed"
            );
            slot.borrow_mut().replace(Box::new(observer));
        });
        let _clear = ClearObserver;
        operation()
    }

    fn open_lock_candidate(
        store: &PrivateCredentialStore,
        target: LockTarget,
    ) -> Result<(File, bool), PrivateStoreError> {
        match rustix::fs::openat(
            &store.directory,
            target.name(store),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(descriptor) => Ok((File::from(descriptor), true)),
            Err(rustix::io::Errno::EXIST) => Ok((open_current_lock(store, target)?, false)),
            Err(_) => Err(PrivateStoreError::UnsafeLock),
        }
    }

    fn open_current_lock(
        store: &PrivateCredentialStore,
        target: LockTarget,
    ) -> Result<File, PrivateStoreError> {
        let descriptor = rustix::fs::openat(
            &store.directory,
            target.name(store),
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| PrivateStoreError::UnsafeLock)?;
        let file = File::from(descriptor);
        validate_file(
            &file.metadata().map_err(|_| PrivateStoreError::UnsafeLock)?,
            true,
        )?;
        Ok(file)
    }

    fn same_file_identity(first: &Metadata, second: &Metadata) -> bool {
        first.dev() == second.dev() && first.ino() == second.ino()
    }

    pub(super) fn verify_lock_authority(
        store: &PrivateCredentialStore,
        lock: &StoreLock,
    ) -> Result<(), PrivateStoreError> {
        verify_explicit_path_authority(store)?;
        if let Some(directory) = &lock.directory {
            let retained_directory = store
                .directory
                .metadata()
                .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
            let locked_directory = directory
                .metadata()
                .map_err(|_| PrivateStoreError::UnsafeDirectory)?;
            if !same_file_identity(&retained_directory, &locked_directory) {
                return Err(PrivateStoreError::UnsafeDirectory);
            }
        }
        let locked_file = lock
            .file
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeLock)?;
        validate_file(&locked_file, true)?;
        let current_file = open_current_lock(store, lock.target)?;
        let current_metadata = current_file
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeLock)?;
        if !same_file_identity(&locked_file, &current_metadata) {
            return Err(PrivateStoreError::UnsafeLock);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn lock_with_test_observer(
        store: &PrivateCredentialStore,
        observer: impl FnMut(usize) -> Result<(), PrivateStoreError>,
    ) -> Result<StoreLock, PrivateStoreError> {
        lock_with_mode_observer(
            store,
            LockTarget::Record,
            LockMode::Exclusive,
            true,
            observer,
        )?
        .ok_or(PrivateStoreError::LockFailed)
    }

    /// Which private lock file a [`StoreLock`] holds.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum LockTarget {
        /// `.<record>.lock`: writers exclusive, readers and leases shared.
        Record,
        /// `.<record>.refresh.lock`: owned exclusively by one refresh claim.
        Execution,
    }

    impl LockTarget {
        fn name(self, store: &PrivateCredentialStore) -> &OsString {
            match self {
                Self::Record => &store.lock_name,
                Self::Execution => &store.execution_lock_name,
            }
        }
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum LockMode {
        Exclusive,
        Shared,
        TryShared,
        TryExclusive,
    }

    impl LockMode {
        const fn is_nonblocking(self) -> bool {
            match self {
                Self::TryShared | Self::TryExclusive => true,
                Self::Exclusive | Self::Shared => false,
            }
        }

        const fn operation(self) -> FlockOperation {
            match self {
                Self::Exclusive => FlockOperation::LockExclusive,
                Self::Shared => FlockOperation::LockShared,
                Self::TryShared => FlockOperation::NonBlockingLockShared,
                Self::TryExclusive => FlockOperation::NonBlockingLockExclusive,
            }
        }
    }

    pub(super) fn load_record(
        store: &PrivateCredentialStore,
    ) -> Result<Option<CredentialRecord>, PrivateStoreError> {
        let record = load_record_for_local_retirement(store)?;
        match record.as_ref().map(|value| {
            (
                value.profile_id().age_against(&store.profile_id),
                value.status(),
            )
        }) {
            None | Some((RecordAge::Current, _)) => Ok(record),
            // A newer build's tombstone carries no login to lose.
            Some((RecordAge::Older, _) | (RecordAge::Newer, CredentialStatus::Revoked)) => {
                Err(PrivateStoreError::ForeignProfile)
            }
            Some((RecordAge::Newer, _)) => Err(PrivateStoreError::WrittenByNewerBuild),
        }
    }

    pub(super) fn load_record_for_local_retirement(
        store: &PrivateCredentialStore,
    ) -> Result<Option<CredentialRecord>, PrivateStoreError> {
        let descriptor = match rustix::fs::openat(
            &store.directory,
            &store.record_name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err(PrivateStoreError::UnsafeRecord),
        };
        let file = File::from(descriptor);
        let before = file
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeRecord)?;
        validate_file(&before, false)?;
        if before.len() > MAX_RECORD_FILE_BYTES {
            return Err(PrivateStoreError::RecordTooLarge);
        }
        let capacity =
            usize::try_from(before.len()).map_err(|_| PrivateStoreError::RecordTooLarge)?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
        (&file)
            .take(MAX_RECORD_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| PrivateStoreError::Io)?;
        if u64::try_from(bytes.len()).map_or(true, |length| length > MAX_RECORD_FILE_BYTES) {
            return Err(PrivateStoreError::RecordTooLarge);
        }
        let after = file
            .metadata()
            .map_err(|_| PrivateStoreError::UnsafeRecord)?;
        validate_file(&after, false)?;
        if !same_file_state(&before, &after) || u64::try_from(bytes.len()).ok() != Some(after.len())
        {
            return Err(PrivateStoreError::UnsafeRecord);
        }
        let record = decode_record(store, &bytes)?;
        Ok(Some(record))
    }

    /// Durably removes the record; local retirement of a record whose
    /// envelope this build cannot decode.
    pub(super) fn remove_record(store: &PrivateCredentialStore) -> Result<(), PrivateStoreError> {
        rustix::fs::unlinkat(&store.directory, &store.record_name, AtFlags::empty())
            .map_err(|_| PrivateStoreError::Io)?;
        store
            .directory
            .sync_all()
            .map_err(|_| PrivateStoreError::Io)
    }

    pub(super) fn write_record(
        store: &PrivateCredentialStore,
        record: &CredentialRecord,
    ) -> Result<(), PrivateStoreError> {
        let bytes = encode_record(store, record)?;
        let temporary_name = temporary_name(&store.record_name)?;
        let descriptor = rustix::fs::openat(
            &store.directory,
            &temporary_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| PrivateStoreError::Io)?;
        let mut temporary = File::from(descriptor);
        let result = (|| {
            rustix::fs::fchmod(&temporary, Mode::RUSR | Mode::WUSR)
                .map_err(|_| PrivateStoreError::Io)?;
            validate_file(
                &temporary
                    .metadata()
                    .map_err(|_| PrivateStoreError::UnsafeRecord)?,
                false,
            )?;
            temporary
                .write_all(&bytes)
                .map_err(|_| PrivateStoreError::Io)?;
            temporary.flush().map_err(|_| PrivateStoreError::Io)?;
            temporary.sync_all().map_err(|_| PrivateStoreError::Io)?;
            rustix::fs::renameat(
                &store.directory,
                &temporary_name,
                &store.directory,
                &store.record_name,
            )
            .map_err(|_| PrivateStoreError::Io)?;
            store
                .directory
                .sync_all()
                .map_err(|_| PrivateStoreError::Io)?;
            validate_installed_record(store)
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&store.directory, &temporary_name, AtFlags::empty());
        }
        result
    }

    fn validate_installed_record(store: &PrivateCredentialStore) -> Result<(), PrivateStoreError> {
        let descriptor = rustix::fs::openat(
            &store.directory,
            &store.record_name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| PrivateStoreError::UnsafeRecord)?;
        let file = File::from(descriptor);
        validate_file(
            &file
                .metadata()
                .map_err(|_| PrivateStoreError::UnsafeRecord)?,
            false,
        )
    }

    fn validate_directory(metadata: &Metadata) -> Result<(), PrivateStoreError> {
        if !metadata.file_type().is_dir() {
            return Err(PrivateStoreError::UnsafeDirectory);
        }
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(PrivateStoreError::UnsafeOwnership);
        }
        if metadata.mode() & 0o7777 != 0o700 {
            return Err(PrivateStoreError::UnsafePermissions);
        }
        Ok(())
    }

    fn validate_file(metadata: &Metadata, lock: bool) -> Result<(), PrivateStoreError> {
        let unsafe_kind = if lock {
            PrivateStoreError::UnsafeLock
        } else {
            PrivateStoreError::UnsafeRecord
        };
        if !metadata.file_type().is_file() || metadata.nlink() != 1 {
            return Err(unsafe_kind);
        }
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(PrivateStoreError::UnsafeOwnership);
        }
        if metadata.mode() & 0o7777 != 0o600 {
            return Err(PrivateStoreError::UnsafePermissions);
        }
        Ok(())
    }

    fn same_file_state(before: &Metadata, after: &Metadata) -> bool {
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.uid() == after.uid()
            && before.mode() == after.mode()
            && before.nlink() == after.nlink()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
    }

    fn temporary_name(record_name: &std::ffi::OsStr) -> Result<OsString, PrivateStoreError> {
        let mut random = [0_u8; 18];
        getrandom::fill(&mut random).map_err(|_| PrivateStoreError::RandomnessUnavailable)?;
        let suffix = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        random.zeroize();
        let record = record_name.to_str().ok_or(PrivateStoreError::InvalidPath)?;
        Ok(OsString::from(format!(".{record}.tmp.{suffix}")))
    }

    pub(super) struct StoreLock {
        file: File,
        directory: Option<File>,
        target: LockTarget,
    }

    impl StoreLock {
        /// Device/inode identity of the held lock file.
        pub(super) fn file_identity(&self) -> Result<StoreAuthority, PrivateStoreError> {
            let metadata = self
                .file
                .metadata()
                .map_err(|_| PrivateStoreError::UnsafeLock)?;
            Ok(StoreAuthority {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
    }

    impl Drop for StoreLock {
        fn drop(&mut self) {
            let _ = rustix::fs::flock(&self.file, FlockOperation::Unlock);
            if let Some(directory) = &self.directory {
                let _ = rustix::fs::flock(directory, FlockOperation::Unlock);
            }
        }
    }
}

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
mod platform {
    use super::*;

    /// Uninhabited: no store handle can be constructed on this target.
    pub(super) type DirectoryHandle = std::convert::Infallible;

    pub(super) struct StoreLock;

    pub(super) fn open_private_directory(
        _path: &Path,
    ) -> Result<(DirectoryHandle, StoreAuthority), PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn lock(_store: &PrivateCredentialStore) -> Result<StoreLock, PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn lock_shared(
        _store: &PrivateCredentialStore,
        _nonblocking: bool,
    ) -> Result<Option<StoreLock>, PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    impl StoreLock {
        pub(super) fn file_identity(&self) -> Result<StoreAuthority, PrivateStoreError> {
            Err(PrivateStoreError::UnsupportedPlatform)
        }
    }

    pub(super) fn try_lock_execution(
        _store: &PrivateCredentialStore,
    ) -> Result<Option<StoreLock>, PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn execution_lock_held(
        _store: &PrivateCredentialStore,
    ) -> Result<bool, PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn try_lock(
        _store: &PrivateCredentialStore,
    ) -> Result<Option<StoreLock>, PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn lock_record_for_claim(
        _store: &PrivateCredentialStore,
        _nonblocking: bool,
    ) -> Result<Option<StoreLock>, PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn verify_lock_authority(
        _store: &PrivateCredentialStore,
        _lock: &StoreLock,
    ) -> Result<(), PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn load_record(
        _store: &PrivateCredentialStore,
    ) -> Result<Option<CredentialRecord>, PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn load_record_for_local_retirement(
        _store: &PrivateCredentialStore,
    ) -> Result<Option<CredentialRecord>, PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn remove_record(_store: &PrivateCredentialStore) -> Result<(), PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
    }

    pub(super) fn write_record(
        _store: &PrivateCredentialStore,
        _record: &CredentialRecord,
    ) -> Result<(), PrivateStoreError> {
        Err(PrivateStoreError::UnsupportedPlatform)
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
mod tests {
    use std::fs;
    use std::fs::OpenOptions;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _, symlink};
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    const LOCK_HELPER_PATH: &str = "PLAKOUS_AUTH_TEST_LOCK_PATH";
    const LOCK_HELPER_READY: &str = "PLAKOUS_AUTH_TEST_LOCK_READY";
    const LOCK_HELPER_RELEASE: &str = "PLAKOUS_AUTH_TEST_LOCK_RELEASE";
    const LOCK_HELPER_SHARED_LEASE: &str = "PLAKOUS_AUTH_TEST_SHARED_LEASE";

    struct Fixture {
        directory: TempDir,
        path: PathBuf,
        profile: SubscriptionIntegrationProfileId,
        slot: CredentialSlotId,
        account: AccountBinding,
    }

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

    struct RotatedDirectory {
        original: PathBuf,
        displaced: PathBuf,
    }

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

    impl Drop for RotatedDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.original);
            let _ = fs::rename(&self.displaced, &self.original);
        }
    }

    fn load_raw_for_test(
        store: &PrivateCredentialStore,
    ) -> Result<Option<CredentialRecord>, PrivateStoreError> {
        store.with_lock(platform::load_record)
    }

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

    #[test]
    fn lifecycle_is_monotonic_and_logout_is_a_tombstone() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"one")?)?;
        let loaded = store.load()?.ok_or("missing record")?;
        assert_eq!(
            store.acquire_ready(&loaded)?.payload().expose_secret(),
            b"one"
        );

        store.replace(
            CredentialGeneration::new(1)?,
            &fixture.record(2, CredentialStatus::RefreshRequired, b"two")?,
        )?;
        assert_eq!(
            store.replace(
                CredentialGeneration::new(2)?,
                &fixture.record(2, CredentialStatus::Ready, b"rollback")?,
            ),
            Err(PrivateStoreError::GenerationRollback)
        );

        let revoked_generation = store.logout(CredentialGeneration::new(2)?)?;
        let revoked = store.load()?.ok_or("missing tombstone")?;
        assert_eq!(revoked_generation.get(), 3);
        assert_eq!(revoked.status(), CredentialStatus::Revoked);
        assert!(
            load_raw_for_test(&store)?
                .ok_or("missing tombstone")?
                .payload()
                .is_empty()
        );
        assert_eq!(
            store.replace(
                CredentialGeneration::new(2)?,
                &fixture.record(4, CredentialStatus::Ready, b"stale")?,
            ),
            Err(PrivateStoreError::StaleWriter)
        );
        Ok(())
    }

    #[test]
    fn refresh_intent_is_crash_safe_quarantine() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        let generation = CredentialGeneration::new(1)?;
        store.initialize(&fixture.record(1, CredentialStatus::RefreshRequired, b"refresh")?)?;

        let intent = store.mark_refresh_intent(generation)?;
        let claim_generation = CredentialGeneration::new(2)?;
        assert_eq!(intent.generation(), claim_generation);
        assert_eq!(intent.status(), CredentialStatus::RefreshOutcomeUnknown);
        assert!(matches!(
            store.mark_refresh_intent(generation),
            Err(PrivateStoreError::StaleWriter)
        ));
        assert!(matches!(
            store.mark_refresh_intent(claim_generation),
            Err(PrivateStoreError::CredentialQuarantined)
        ));
        assert_eq!(
            store.replace(
                generation,
                &fixture.record(3, CredentialStatus::Ready, b"stale-success")?,
            ),
            Err(PrivateStoreError::StaleWriter)
        );
        drop(store);

        let restarted = fixture.store()?;
        let quarantined = restarted.load()?.ok_or("missing quarantine")?;
        assert_eq!(quarantined.generation(), claim_generation);
        assert_eq!(
            quarantined.status(),
            CredentialStatus::RefreshOutcomeUnknown
        );
        assert_eq!(
            restarted
                .quarantine_refresh_outcome(claim_generation)?
                .status(),
            CredentialStatus::RefreshOutcomeUnknown
        );
        assert_eq!(
            restarted
                .quarantine_refresh_outcome(claim_generation)?
                .generation(),
            claim_generation
        );
        Ok(())
    }

    #[test]
    fn generic_cas_cannot_clear_a_dropped_claim_after_restart()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
        let ready: CredentialFence = store.load()?.ok_or("missing ready fence")?;
        let claim = store.begin_refresh(&ready)?;
        let unknown = claim.fence();
        drop(claim);
        drop(store);

        let restarted = fixture.store()?;
        let replacement = fixture.record(3, CredentialStatus::Ready, b"new")?;
        assert_eq!(
            restarted.replace(unknown.generation(), &replacement),
            Err(PrivateStoreError::CredentialQuarantined)
        );
        assert_eq!(
            restarted.replace(unknown.generation(), &replacement),
            Err(PrivateStoreError::CredentialQuarantined)
        );
        assert_eq!(restarted.load()?.ok_or("quarantine was lost")?, unknown);
        Ok(())
    }

    #[test]
    fn dropping_a_started_claim_during_unwind_leaves_unknown()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
        let ready = store.load()?.ok_or("missing ready fence")?;
        let claim = store.begin_refresh(&ready)?;
        let unknown = claim.fence();
        let unwind = std::panic::catch_unwind(move || {
            let started = claim.start();
            assert_eq!(started.payload().expose_secret(), b"old");
            panic!("simulated provider panic");
        });
        assert!(unwind.is_err());
        drop(store);
        assert_eq!(
            fixture.store()?.load()?.ok_or("quarantine was lost")?,
            unknown
        );
        Ok(())
    }

    #[test]
    fn concurrent_refresh_intent_has_one_generation_claimant()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.store()?.initialize(&fixture.record(
            1,
            CredentialStatus::RefreshRequired,
            b"refresh",
        )?)?;
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for store in [fixture.store()?, fixture.store()?] {
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                store.mark_refresh_intent(
                    CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
                )
            }));
        }
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().map_err(|_| "refresh claimant panicked"))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        // The loser either observed the winner's claim (stale generation) or
        // found the execution lock owned (busy, nothing written); both are
        // local facts and neither is a second claimant.
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(PrivateStoreError::StaleWriter | PrivateStoreError::StoreBusy)
                ))
                .count(),
            1
        );
        let claimed = fixture.store()?.load()?.ok_or("missing claim")?;
        assert_eq!(claimed.generation(), CredentialGeneration::new(2)?);
        assert_eq!(claimed.status(), CredentialStatus::RefreshOutcomeUnknown);
        Ok(())
    }

    #[test]
    fn ready_leases_share_and_block_logout_until_all_drop() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"lease-secret")?)?;
        let fence = store.load()?.ok_or("missing record")?;
        let first = store.acquire_ready(&fence)?;
        let second = fixture.store()?.try_acquire_ready(&fence)?.ok_or("busy")?;
        assert_eq!(fixture.store()?.try_load()?, Some(fence.clone()));
        assert_eq!(first.payload().expose_secret(), b"lease-secret");
        assert!(!format!("{fence:?}").contains("opaque-account"));
        assert!(!format!("{first:?}").contains("lease-secret"));

        let mutation_store = fixture.store()?;
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = sender.send(mutation_store.logout(
                CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
            ));
            Ok::<(), PrivateStoreError>(())
        });
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));
        drop(first);
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));
        drop(second);
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5))??,
            CredentialGeneration::new(2)?
        );
        worker.join().map_err(|_| "logout worker panicked")??;
        Ok(())
    }

    #[test]
    fn nonblocking_fence_read_names_writer_contention() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"fixture-secret")?)?;
        let writer = platform::lock(&store)?;
        assert_eq!(store.try_load(), Err(PrivateStoreError::StoreBusy));
        drop(writer);
        assert!(store.try_load()?.is_some());
        Ok(())
    }

    #[test]
    fn refresh_claim_in_flight_probe_tracks_the_claim_without_writing()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"probe-secret")?)?;
        let execution_lock = store.path().with_file_name(&store.execution_lock_name);
        let _ = fs::remove_file(&execution_lock);
        assert!(!store.refresh_claim_in_flight()?);
        assert!(
            !execution_lock.exists(),
            "the probe must not create a lock file"
        );
        let fence = store.load()?.ok_or("missing ready")?;
        let claim = fixture.store()?.begin_refresh(&fence)?.start();
        let unknown = store.load()?.ok_or("missing unknown")?;
        assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
        assert!(store.refresh_claim_in_flight()?);
        assert!(!store.refresh_outcome_settled(&unknown)?);
        assert_eq!(store.load()?, Some(unknown.clone()));
        drop(claim);
        assert!(!store.refresh_claim_in_flight()?);
        assert!(store.refresh_outcome_settled(&unknown)?);
        assert!(!store.refresh_outcome_settled(&fence)?);
        assert_eq!(store.load()?, Some(unknown));
        Ok(())
    }

    #[test]
    fn try_restore_unknown_to_ready_never_waits_and_restores_only_the_exact_fence()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"restore-secret")?)?;
        let ready = store.load()?.ok_or("missing ready")?;
        let claim = fixture.store()?.begin_refresh(&ready)?.start();
        let unknown = claim.fence();
        assert_eq!(store.try_restore_unknown_to_ready(&unknown)?, None);
        assert_eq!(store.load()?, Some(unknown.clone()));
        drop(claim);
        assert!(matches!(
            store.try_restore_unknown_to_ready(&ready),
            Err(PrivateStoreError::FenceMismatch)
        ));
        let restored = store
            .try_restore_unknown_to_ready(&unknown)?
            .ok_or("uncontended restore")?;
        assert_eq!(restored.status(), CredentialStatus::Ready);
        assert_eq!(restored.generation(), unknown.generation().next()?);
        assert_eq!(store.load()?, Some(restored));
        Ok(())
    }

    #[test]
    fn refresh_claim_completion_restore_drop_and_privacy() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old-secret")?)?;
        let ready = store.load()?.ok_or("missing ready")?;
        let claim = store.begin_refresh(&ready)?;
        assert_eq!(claim.previous_generation(), CredentialGeneration::new(1)?);
        assert_eq!(claim.previous_status(), CredentialStatus::Ready);
        assert_eq!(claim.payload().expose_secret(), b"old-secret");
        assert!(!format!("{claim:?}").contains("old-secret"));
        let claim = claim.start();
        assert_eq!(claim.payload().expose_secret(), b"old-secret");
        assert!(!format!("{claim:?}").contains("old-secret"));
        let completed = store.complete_refresh(
            claim,
            fixture.record(3, CredentialStatus::Ready, b"new-secret")?,
        )?;
        assert_eq!(completed.generation(), CredentialGeneration::new(3)?);
        assert_eq!(
            store.acquire_ready(&completed)?.payload().expose_secret(),
            b"new-secret"
        );

        store.replace(
            CredentialGeneration::new(3)?,
            &fixture.record(4, CredentialStatus::RefreshRequired, b"restore-secret")?,
        )?;
        let refresh_required = store.load()?.ok_or("missing refresh record")?;
        let restore_claim = store.begin_refresh(&refresh_required)?;
        let restored = store.restore_refresh_definitely_not_started(restore_claim)?;
        assert_eq!(restored.generation(), CredentialGeneration::new(6)?);
        let restored_record = store.load()?.ok_or("missing restored record")?;
        assert_eq!(restored_record.status(), CredentialStatus::RefreshRequired);
        assert_eq!(
            load_raw_for_test(&store)?
                .ok_or("missing restored record")?
                .payload()
                .expose_secret(),
            b"restore-secret"
        );

        let dropped_claim = store.begin_refresh(&restored_record)?;
        let dropped_fence = dropped_claim.fence();
        drop(dropped_claim);
        let quarantined = store.load()?.ok_or("missing dropped claim")?;
        assert_eq!(quarantined, dropped_fence);
        assert_eq!(
            quarantined.status(),
            CredentialStatus::RefreshOutcomeUnknown
        );
        Ok(())
    }

    #[test]
    fn begin_refresh_reserves_unknown_and_completion_generations_before_writing()
    -> Result<(), Box<dyn std::error::Error>> {
        for generation in [u64::MAX - 1, u64::MAX] {
            let fixture = Fixture::new()?;
            let store = fixture.store()?;
            store.initialize(&fixture.record(
                generation,
                CredentialStatus::RefreshRequired,
                b"boundary-old",
            )?)?;
            let before = store.load()?.ok_or("missing boundary record")?;
            assert_eq!(
                store.begin_refresh(&before).err(),
                Some(PrivateStoreError::GenerationExhausted)
            );
            let unchanged = load_raw_for_test(&store)?.ok_or("boundary record was lost")?;
            assert_eq!(unchanged.fence(), before);
            assert_eq!(unchanged.status(), CredentialStatus::RefreshRequired);
            assert_eq!(unchanged.payload().expose_secret(), b"boundary-old");
        }

        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(
            u64::MAX - 2,
            CredentialStatus::RefreshRequired,
            b"last-refresh",
        )?)?;
        let before = store.load()?.ok_or("missing final refresh record")?;
        let claim = store.begin_refresh(&before)?;
        assert_eq!(claim.fence().generation().get(), u64::MAX - 1);
        let completed = store.complete_refresh(
            claim.start(),
            fixture.record(u64::MAX, CredentialStatus::Ready, b"last-ready")?,
        )?;
        assert_eq!(completed.generation().get(), u64::MAX);
        Ok(())
    }

    #[test]
    fn complete_refresh_rejects_skipped_generation_and_leaves_unknown()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(11, CredentialStatus::Ready, b"old")?)?;
        let ready = store.load()?.ok_or("missing ready record")?;
        let claim = store.begin_refresh(&ready)?;
        let unknown = claim.fence();
        assert_eq!(
            store.complete_refresh(
                claim.start(),
                fixture.record(14, CredentialStatus::Ready, b"skipped")?,
            ),
            Err(PrivateStoreError::GenerationNotSequential)
        );
        let unchanged = load_raw_for_test(&store)?.ok_or("missing quarantine")?;
        assert_eq!(unchanged.fence(), unknown);
        assert_eq!(unchanged.status(), CredentialStatus::RefreshOutcomeUnknown);
        assert_eq!(unchanged.payload().expose_secret(), b"old");
        Ok(())
    }

    #[test]
    fn begin_refresh_rejects_unknown_by_default() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
        let ready = store.load()?.ok_or("missing ready record")?;
        let claim = store.begin_refresh(&ready)?;
        drop(claim);
        let unknown = store.load()?.ok_or("missing unknown record")?;
        assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
        assert_eq!(
            store.begin_refresh(&unknown).err(),
            Some(PrivateStoreError::CredentialQuarantined)
        );
        Ok(())
    }

    #[test]
    fn begin_refresh_reclaiming_unknown_claims_from_unknown()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
        let ready = store.load()?.ok_or("missing ready record")?;
        let claim = store.begin_refresh(&ready)?;
        let unknown = claim.fence();
        drop(claim);
        let reclaim = store.begin_refresh_reclaiming_unknown(&unknown)?;
        assert_eq!(reclaim.fence().generation().get(), 3);
        assert_eq!(reclaim.payload().expose_secret(), b"old");
        let loaded = store.load()?.ok_or("missing reclaimed unknown")?;
        assert_eq!(loaded.status(), CredentialStatus::RefreshOutcomeUnknown);
        assert_eq!(loaded.generation().get(), 3);
        Ok(())
    }

    #[test]
    fn restore_unknown_to_ready_writes_ready_next_generation_same_payload()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
        let ready = store.load()?.ok_or("missing ready record")?;
        let claim = store.begin_refresh(&ready)?;
        let unknown = claim.fence();
        drop(claim);
        let restored = store.restore_unknown_to_ready(&unknown)?;
        assert_eq!(restored.status(), CredentialStatus::Ready);
        assert_eq!(restored.generation().get(), 3);
        let loaded = store.load()?.ok_or("missing restored ready")?;
        assert_eq!(loaded, restored);
        let raw = load_raw_for_test(&store)?.ok_or("missing restored record")?;
        assert_eq!(raw.payload().expose_secret(), b"old");
        Ok(())
    }

    #[test]
    fn stale_and_wrong_refresh_claims_cannot_clear_unknown()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = Fixture::new()?;
        let second = Fixture::new()?;
        for fixture in [&first, &second] {
            fixture.store()?.initialize(&fixture.record(
                1,
                CredentialStatus::Ready,
                b"same-old",
            )?)?;
        }
        let first_store = first.store()?;
        let second_store = second.store()?;
        let first_fence = first_store.load()?.ok_or("missing first")?;
        let second_fence = second_store.load()?.ok_or("missing second")?;
        let wrong_claim = first_store.begin_refresh(&first_fence)?;
        let second_claim = second_store.begin_refresh(&second_fence)?;
        assert!(matches!(
            second_store.complete_refresh(
                wrong_claim.start(),
                second.record(3, CredentialStatus::Ready, b"wrong")?,
            ),
            Err(PrivateStoreError::RefreshClaimMismatch)
        ));
        assert_eq!(
            second_store.load()?.ok_or("second lost")?.status(),
            CredentialStatus::RefreshOutcomeUnknown
        );

        // A live claim owns the private directory, so no writer can replace
        // the record through the API; inject the changed Unknown directly to
        // prove the claim's own record verification still refuses it.
        platform::write_record(
            &second_store,
            &second.record(3, CredentialStatus::RefreshOutcomeUnknown, b"other-old")?,
        )?;
        assert!(matches!(
            second_store.complete_refresh(
                second_claim.start(),
                second.record(3, CredentialStatus::Ready, b"new")?,
            ),
            Err(PrivateStoreError::RefreshClaimMismatch)
        ));
        let still_unknown = second_store.load()?.ok_or("stale claim cleared")?;
        assert_eq!(still_unknown.generation(), CredentialGeneration::new(3)?);
        assert_eq!(
            still_unknown.status(),
            CredentialStatus::RefreshOutcomeUnknown
        );
        Ok(())
    }

    #[test]
    fn concurrent_initialization_has_one_explicit_winner() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for payload in [b"first".as_slice(), b"second".as_slice()] {
            let store = fixture.store()?;
            let barrier = Arc::clone(&barrier);
            let profile = fixture.profile.clone();
            let account = fixture.account.clone();
            workers.push(thread::spawn(move || {
                let candidate = CredentialRecord::new(
                    profile,
                    account,
                    CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
                    CredentialStatus::Ready,
                    SecretBytes::new(payload.to_vec()),
                )
                .map_err(|_| PrivateStoreError::InvalidRecord)?;
                barrier.wait();
                store.initialize_selected(&candidate)
            }));
        }
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().map_err(|_| "initialization worker panicked"))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Err(PrivateStoreError::AlreadyInitialized)))
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn independent_handles_reject_a_cas_race() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture
            .store()?
            .initialize(&fixture.record(1, CredentialStatus::Ready, b"initial")?)?;
        let first = fixture.store()?;
        let second = fixture.store()?;
        let profile = fixture.profile.clone();
        let account = fixture.account.clone();
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for (store, payload) in [(first, b"first".as_slice()), (second, b"second".as_slice())] {
            let barrier = Arc::clone(&barrier);
            let profile = profile.clone();
            let account = account.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                let record = CredentialRecord::new(
                    profile,
                    account,
                    CredentialGeneration::new(2).map_err(|_| PrivateStoreError::InvalidRecord)?,
                    CredentialStatus::Ready,
                    SecretBytes::new(payload.to_vec()),
                )
                .map_err(|_| PrivateStoreError::InvalidRecord)?;
                store.replace(
                    CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
                    &record,
                )
            }));
        }
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().map_err(|_| "CAS worker panicked"))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(PrivateStoreError::StaleWriter))
                .count(),
            1
        );
        assert_eq!(
            fixture
                .store()?
                .load()?
                .ok_or("missing winner")?
                .generation(),
            CredentialGeneration::new(2)?
        );
        Ok(())
    }

    #[test]
    fn rejects_unsafe_permissions_symlinks_and_hardlinks() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        fs::set_permissions(fixture.directory.path(), fs::Permissions::from_mode(0o755))?;
        assert!(matches!(
            fixture.store(),
            Err(PrivateStoreError::UnsafePermissions)
        ));
        fs::set_permissions(fixture.directory.path(), fs::Permissions::from_mode(0o700))?;

        let outside = fixture.directory.path().join("outside");
        fs::write(&outside, b"{}")?;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))?;
        symlink(&outside, &fixture.path)?;
        assert!(matches!(
            fixture.store()?.load(),
            Err(PrivateStoreError::UnsafeRecord)
        ));
        fs::remove_file(&fixture.path)?;

        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"payload")?)?;
        let alias = fixture.directory.path().join("hardlink");
        fs::hard_link(&fixture.path, &alias)?;
        assert!(matches!(store.load(), Err(PrivateStoreError::UnsafeRecord)));
        fs::remove_file(alias)?;

        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o644))?;
        assert!(matches!(
            store.load(),
            Err(PrivateStoreError::UnsafePermissions)
        ));
        Ok(())
    }

    #[test]
    fn rejects_unsafe_lock_file() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        let outside = fixture.directory.path().join("outside-lock");
        fs::write(&outside, b"")?;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))?;
        symlink(&outside, store.lock_path())?;
        assert!(matches!(store.load(), Err(PrivateStoreError::UnsafeLock)));
        fs::remove_file(store.lock_path())?;

        fs::hard_link(&outside, store.lock_path())?;
        assert!(matches!(store.load(), Err(PrivateStoreError::UnsafeLock)));
        Ok(())
    }

    #[test]
    fn rejects_symlink_in_private_directory_path() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        let actual = root.path().join("actual");
        fs::create_dir(&actual)?;
        fs::set_permissions(&actual, fs::Permissions::from_mode(0o700))?;
        let alias = root.path().join("alias");
        symlink(&actual, &alias)?;
        let result = PrivateCredentialStore::open_slot(
            alias.join("credentials.json"),
            SubscriptionIntegrationProfileId::new("profile")?,
            CredentialSlotId::new("primary")?,
        );
        assert!(matches!(result, Err(PrivateStoreError::UnsafeDirectory)));
        Ok(())
    }

    #[test]
    fn existing_handles_reject_split_directory_authority_for_cas_and_lease()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
        let ready = store.load()?.ok_or("missing ready fence")?;
        let replacement = fixture.record(2, CredentialStatus::Ready, b"new")?;

        let rotation = RotatedDirectory::replace(fixture.directory.path())?;
        assert_eq!(
            store.replace(CredentialGeneration::new(1)?, &replacement),
            Err(PrivateStoreError::UnsafeDirectory)
        );
        assert!(matches!(
            store.acquire_ready(&ready),
            Err(PrivateStoreError::UnsafeDirectory)
        ));
        drop(rotation);

        assert_eq!(
            store.load()?.ok_or("original authority was not restored")?,
            ready
        );
        Ok(())
    }

    #[test]
    fn blocked_load_rejects_directory_rotation_after_flock()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture
            .store()?
            .initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
        let before = fs::read(&fixture.path)?;
        let (result, rotation) =
            run_after_blocked_directory_rotation(&fixture, |store| store.load().map(drop))?;
        assert_eq!(result, Err(PrivateStoreError::UnsafeDirectory));
        assert_eq!(
            fs::read(rotation.displaced.join("credentials.json"))?,
            before
        );
        Ok(())
    }

    #[test]
    fn blocked_initialize_cannot_write_detached_directory() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let candidate = fixture.record(1, CredentialStatus::Ready, b"new")?;
        let (result, rotation) = run_after_blocked_directory_rotation(&fixture, move |store| {
            store.initialize(&candidate)
        })?;
        assert_eq!(result, Err(PrivateStoreError::UnsafeDirectory));
        assert!(!rotation.displaced.join("credentials.json").exists());
        assert!(!fixture.path.exists());
        Ok(())
    }

    #[test]
    fn blocked_cas_cannot_write_detached_directory() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture
            .store()?
            .initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
        let before = fs::read(&fixture.path)?;
        let replacement = fixture.record(2, CredentialStatus::Ready, b"new")?;
        let (result, rotation) = run_after_blocked_directory_rotation(&fixture, move |store| {
            store.replace(
                CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
                &replacement,
            )
        })?;
        assert_eq!(result, Err(PrivateStoreError::UnsafeDirectory));
        assert_eq!(
            fs::read(rotation.displaced.join("credentials.json"))?,
            before
        );
        Ok(())
    }

    #[test]
    fn blocked_ready_lease_rejects_detached_directory() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
        let fence = store.load()?.ok_or("missing ready fence")?;
        let (result, _rotation) = run_after_blocked_directory_rotation(&fixture, move |store| {
            store.acquire_ready(&fence).map(drop)
        })?;
        assert_eq!(result, Err(PrivateStoreError::UnsafeDirectory));
        Ok(())
    }

    #[test]
    fn lock_retries_when_open_inode_is_replaced_before_flock()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        assert!(store.load()?.is_none());
        let lock_path = store.lock_path();
        let displaced = fixture.directory.path().join("displaced-lock");
        let mut observations = 0_usize;
        let _guard = platform::lock_with_test_observer(&store, |attempt| {
            observations += 1;
            if attempt == 0 {
                fs::rename(&lock_path, &displaced).map_err(|_| PrivateStoreError::Io)?;
                create_private_file(&lock_path)?;
            }
            Ok(())
        })?;
        assert_eq!(observations, 2);
        assert_ne!(
            fs::metadata(&lock_path)?.ino(),
            fs::metadata(displaced)?.ino()
        );
        Ok(())
    }

    #[test]
    fn lock_fails_closed_under_continuous_inode_replacement()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        assert!(store.load()?.is_none());
        let lock_path = store.lock_path();
        let result = platform::lock_with_test_observer(&store, |attempt| {
            let displaced = fixture.directory.path().join(format!("lock-{attempt}"));
            fs::rename(&lock_path, displaced).map_err(|_| PrivateStoreError::Io)?;
            create_private_file(&lock_path)
        });
        assert!(matches!(result, Err(PrivateStoreError::UnsafeLock)));
        Ok(())
    }

    #[test]
    fn cross_process_lock_helper() -> Result<(), Box<dyn std::error::Error>> {
        let Some(path) = std::env::var_os(LOCK_HELPER_PATH) else {
            return Ok(());
        };
        let ready = std::env::var_os(LOCK_HELPER_READY).ok_or("missing ready path")?;
        let release = std::env::var_os(LOCK_HELPER_RELEASE).ok_or("missing release path")?;
        let store = PrivateCredentialStore::open_slot(
            PathBuf::from(path),
            SubscriptionIntegrationProfileId::new("profile")?,
            CredentialSlotId::new("primary")?,
        )?;
        if std::env::var_os(LOCK_HELPER_SHARED_LEASE).is_some() {
            let fence = store.load()?.ok_or("missing helper credential")?;
            let _lease = store.acquire_ready(&fence)?;
            fs::write(ready, b"ready")?;
            wait_for_release(Path::new(&release))
        } else {
            let _guard = platform::lock(&store)?;
            fs::write(ready, b"ready")?;
            wait_for_release(Path::new(&release))
        }
    }

    #[test]
    fn lock_is_exclusive_across_processes() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture
            .store()?
            .initialize(&fixture.record(1, CredentialStatus::Ready, b"payload")?)?;
        let fence = fixture.store()?.load()?.ok_or("missing fence")?;
        let ready = fixture.directory.path().join("helper-ready");
        let release = fixture.directory.path().join("helper-release");
        let executable = std::env::current_exe()?;
        let mut child = Command::new(executable)
            .args([
                "--exact",
                "private_store::tests::cross_process_lock_helper",
                "--nocapture",
            ])
            .env(LOCK_HELPER_PATH, &fixture.path)
            .env(LOCK_HELPER_READY, &ready)
            .env(LOCK_HELPER_RELEASE, &release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        wait_for_file(&ready)?;
        assert!(fixture.store()?.try_acquire_ready(&fence)?.is_none());

        let contender = fixture.store()?;
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = contender.load().map(|record| record.is_some());
            let _ = sender.send(result);
        });
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));
        fs::write(&release, b"release")?;
        assert!(receiver.recv_timeout(Duration::from_secs(2))??);
        worker.join().map_err(|_| "lock contender panicked")?;
        assert!(child.wait()?.success());
        Ok(())
    }

    #[test]
    fn shared_lease_is_cross_process_and_blocks_exclusive_logout()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture
            .store()?
            .initialize(&fixture.record(1, CredentialStatus::Ready, b"payload")?)?;
        let fence = fixture.store()?.load()?.ok_or("missing fence")?;
        let ready = fixture.directory.path().join("shared-helper-ready");
        let release = fixture.directory.path().join("shared-helper-release");
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "private_store::tests::cross_process_lock_helper",
                "--nocapture",
            ])
            .env(LOCK_HELPER_PATH, &fixture.path)
            .env(LOCK_HELPER_READY, &ready)
            .env(LOCK_HELPER_RELEASE, &release)
            .env(LOCK_HELPER_SHARED_LEASE, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        wait_for_file(&ready)?;
        let local_lease = fixture
            .store()?
            .try_acquire_ready(&fence)?
            .ok_or("shared lock was not shared")?;

        let mutation_store = fixture.store()?;
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = mutation_store.logout(
                CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
            );
            let _ = sender.send(result);
            Ok::<(), PrivateStoreError>(())
        });
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));
        drop(local_lease);
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));
        fs::write(&release, b"release")?;
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5))??,
            CredentialGeneration::new(2)?
        );
        worker
            .join()
            .map_err(|_| "cross-process logout panicked")??;
        assert!(child.wait()?.success());
        Ok(())
    }

    #[test]
    fn rejects_foreign_malformed_unknown_and_oversized_records()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        let documents = [
            json!({
                "format_version": 999,
                "profile_id": "profile",
                "slot_id": "primary",
                "account_binding": "opaque-account",
                "generation": 1,
                "status": "ready",
                "payload": "eA=="
            }),
            json!({
                "format_version": 1,
                "profile_id": "profile",
                "account_binding": "opaque-account",
                "generation": 1,
                "status": "ready",
                "payload": "eA=="
            }),
            json!({
                "format_version": 2,
                "profile_id": "profile",
                "slot_id": "primary",
                "account_binding": "opaque-account",
                "generation": 1,
                "status": "ready",
                "payload": "eA==",
                "future_security_field": true
            }),
            json!({
                "format_version": 2,
                "profile_id": "foreign",
                "slot_id": "primary",
                "account_binding": "opaque-account",
                "generation": 1,
                "status": "ready",
                "payload": "eA=="
            }),
            json!({
                "format_version": 2,
                "profile_id": "profile",
                "slot_id": "other-slot",
                "account_binding": "opaque-account",
                "generation": 1,
                "status": "ready",
                "payload": "eA=="
            }),
        ];
        let expected = [
            PrivateStoreError::WrittenByNewerBuild,
            PrivateStoreError::UnsupportedVersion,
            PrivateStoreError::MalformedRecord,
            PrivateStoreError::ForeignProfile,
            PrivateStoreError::ForeignSlot,
        ];
        for (document, expected) in documents.into_iter().zip(expected) {
            fs::write(&fixture.path, serde_json::to_vec(&document)?)?;
            fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600))?;
            assert!(matches!(store.load(), Err(error) if error == expected));
        }

        let oversized_length = usize::try_from(MAX_RECORD_FILE_BYTES)? + 1;
        fs::write(&fixture.path, vec![b'x'; oversized_length])?;
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600))?;
        assert!(matches!(
            store.load(),
            Err(PrivateStoreError::RecordTooLarge)
        ));
        Ok(())
    }

    #[test]
    fn newer_envelopes_and_profiles_are_refused_untouched_and_retire_only_locally()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let current = SubscriptionIntegrationProfileId::new("vendor.product.compat.2026-07-31.r1")?;
        let store = PrivateCredentialStore::open_slot(
            &fixture.path,
            current.clone(),
            fixture.slot.clone(),
        )?;
        let document = |format_version: u32, profile: &str, status: &str| {
            json!({
                "format_version": format_version,
                "profile_id": profile,
                "slot_id": "primary",
                "account_binding": "opaque-account",
                "generation": 4,
                "status": status,
                "payload": if status == "revoked" { "" } else { "eA==" }
            })
        };
        let cases = [
            (
                document(1, current.as_str(), "ready"),
                Some(PrivateStoreError::UnsupportedVersion),
            ),
            (document(2, current.as_str(), "ready"), None),
            (
                document(3, current.as_str(), "ready"),
                Some(PrivateStoreError::WrittenByNewerBuild),
            ),
            (
                document(2, "vendor.product.compat.2026-07-31.r0", "ready"),
                Some(PrivateStoreError::ForeignProfile),
            ),
            (
                document(2, "vendor.product.compat.2026-07-31.r2", "refresh_required"),
                Some(PrivateStoreError::WrittenByNewerBuild),
            ),
            (
                document(
                    2,
                    "vendor.product.compat.2026-08-01.r1",
                    "refresh_outcome_unknown",
                ),
                Some(PrivateStoreError::WrittenByNewerBuild),
            ),
            (
                document(2, "vendor-renamed-profile", "ready"),
                Some(PrivateStoreError::WrittenByNewerBuild),
            ),
            // A newer build's tombstone holds no login, so it is not guarded.
            (
                document(2, "vendor.product.compat.2026-08-01.r1", "revoked"),
                Some(PrivateStoreError::ForeignProfile),
            ),
        ];
        for (document, expected) in cases {
            let bytes = serde_json::to_vec(&document)?;
            fs::write(&fixture.path, &bytes)?;
            fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600))?;
            assert_eq!(store.load().err(), expected, "{document}");
            assert_eq!(store.status().err(), expected);
            assert_eq!(store.inspect_payload(SecretBytes::len).err(), expected);
            if expected == Some(PrivateStoreError::WrittenByNewerBuild) {
                let fence = CredentialFence::new(
                    current.clone(),
                    AccountBinding::new("opaque-account")?,
                    CredentialGeneration::new(4)?,
                    CredentialStatus::Ready,
                );
                assert!(store.begin_refresh(&fence).is_err());
                assert!(store.try_acquire_ready(&fence).is_err());
                assert_eq!(fs::read(&fixture.path)?, bytes, "{document}");
            }
        }

        // Explicit local retirement still works: a newer profile becomes a
        // tombstone, and a newer envelope it cannot decode is removed.
        let newer_profile =
            serde_json::to_vec(&document(2, "vendor.product.compat.2026-08-01.r1", "ready"))?;
        fs::write(&fixture.path, &newer_profile)?;
        assert_eq!(
            store.logout_local()?,
            crate::OfflineCredentialLifecycle::Revoked
        );
        assert_eq!(store.status()?, Some(CredentialStatus::Revoked));
        fs::write(
            &fixture.path,
            serde_json::to_vec(&document(3, current.as_str(), "ready"))?,
        )?;
        assert_eq!(
            store.logout_local()?,
            crate::OfflineCredentialLifecycle::SignedOut
        );
        assert!(!fixture.path.exists());
        assert_eq!(store.status()?, None);
        Ok(())
    }

    #[test]
    fn selected_slot_switches_accounts_only_through_exact_plus_one()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let slot = CredentialSlotId::new("primary")?;
        let store = PrivateCredentialStore::open_slot(
            &fixture.path,
            fixture.profile.clone(),
            slot.clone(),
        )?;
        store.initialize_selected(&fixture.record(1, CredentialStatus::Ready, b"account-a")?)?;
        let bytes = fs::read(&fixture.path)?;
        let document: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(document["format_version"], 2);
        assert_eq!(document["slot_id"], "primary");
        assert_eq!(document["account_binding"], "opaque-account");

        let account_b = AccountBinding::new("actual-account-b")?;
        let before_switch = store.load()?.ok_or("missing selected account")?;
        let switched_record = CredentialRecord::new(
            fixture.profile.clone(),
            account_b.clone(),
            CredentialGeneration::new(2)?,
            CredentialStatus::Ready,
            SecretBytes::new(b"account-b".to_vec()),
        )?;
        let switched = store.replace_selected_login(&before_switch, switched_record)?;
        assert_eq!(switched.account_binding(), &account_b);
        let jumped = CredentialRecord::new(
            fixture.profile.clone(),
            AccountBinding::new("account-c")?,
            CredentialGeneration::new(4)?,
            CredentialStatus::Ready,
            SecretBytes::new(b"account-c".to_vec()),
        )?;
        assert_eq!(
            store.replace_selected_login(&switched, jumped),
            Err(PrivateStoreError::GenerationNotSequential)
        );

        let claim = store.begin_refresh(&switched)?;
        let wrong_account_completion = fixture.record(4, CredentialStatus::Ready, b"wrong")?;
        assert!(matches!(
            store.complete_refresh(claim.start(), wrong_account_completion),
            Err(PrivateStoreError::ForeignAccount)
        ));
        let unknown = store.load()?.ok_or("claim did not persist")?;
        assert_eq!(unknown.account_binding(), &account_b);
        assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
        assert_eq!(
            store.logout(unknown.generation())?,
            CredentialGeneration::new(4)?
        );
        let tombstone = store.load()?.ok_or("missing slot tombstone")?;
        assert_eq!(tombstone.account_binding(), &account_b);
        assert_eq!(tombstone.status(), CredentialStatus::Revoked);
        Ok(())
    }

    #[test]
    fn explicit_local_logout_retires_foreign_selected_profile_without_reusing_payload()
    -> Result<(), Box<dyn std::error::Error>> {
        for state in [
            CredentialStatus::Ready,
            CredentialStatus::RefreshOutcomeUnknown,
            CredentialStatus::Revoked,
        ] {
            let fixture = Fixture::new()?;
            let slot = CredentialSlotId::new("primary")?;
            let old_profile = SubscriptionIntegrationProfileId::new("previous-profile")?;
            let old = PrivateCredentialStore::open_slot(
                &fixture.path,
                old_profile.clone(),
                slot.clone(),
            )?;
            old.initialize_selected(&CredentialRecord::new(
                old_profile,
                fixture.account.clone(),
                CredentialGeneration::new(1)?,
                CredentialStatus::Ready,
                SecretBytes::new(b"old-secret".to_vec()),
            )?)?;
            if state == CredentialStatus::RefreshOutcomeUnknown {
                let fence = old.load()?.ok_or("missing old credential")?;
                drop(old.begin_refresh(&fence)?);
            } else if state == CredentialStatus::Revoked {
                old.logout_local()?;
            }
            let current =
                PrivateCredentialStore::open_slot(&fixture.path, fixture.profile.clone(), slot)?;
            assert_eq!(
                current.load().err(),
                Some(PrivateStoreError::ForeignProfile)
            );
            let before = old.load()?.ok_or("missing old credential")?.generation();
            assert_eq!(
                current.logout_local()?,
                crate::OfflineCredentialLifecycle::Revoked
            );
            let retired = current.load()?.ok_or("missing retired credential")?;
            assert_eq!(retired.generation(), before.next()?);
            assert_eq!(retired.status(), CredentialStatus::Revoked);
            assert_eq!(retired.profile_id(), &fixture.profile);
            assert_eq!(
                load_raw_for_test(&current)?
                    .ok_or("missing retired record")?
                    .payload()
                    .expose_secret(),
                b""
            );
            assert_eq!(old.load().err(), Some(PrivateStoreError::ForeignProfile));
            assert_eq!(
                old.logout(before).err(),
                Some(PrivateStoreError::ForeignProfile)
            );
            assert_eq!(
                current.logout_local()?,
                crate::OfflineCredentialLifecycle::Revoked
            );
            assert_eq!(
                current.load()?.ok_or("missing tombstone")?.generation(),
                retired.generation()
            );
        }
        Ok(())
    }

    #[test]
    fn local_logout_refuses_foreign_slot_malformed_record_and_exhausted_generation()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let old_profile = SubscriptionIntegrationProfileId::new("previous-profile")?;
        let foreign_slot = PrivateCredentialStore::open_slot(
            &fixture.path,
            old_profile.clone(),
            CredentialSlotId::new("other-slot")?,
        )?;
        foreign_slot.initialize_selected(&CredentialRecord::new(
            old_profile,
            fixture.account.clone(),
            CredentialGeneration::new(1)?,
            CredentialStatus::Ready,
            SecretBytes::new(b"old-secret".to_vec()),
        )?)?;
        let current = PrivateCredentialStore::open_slot(
            &fixture.path,
            fixture.profile.clone(),
            CredentialSlotId::new("primary")?,
        )?;
        let original = fs::read(&fixture.path)?;
        assert_eq!(
            current.logout_local().err(),
            Some(PrivateStoreError::ForeignSlot)
        );
        assert_eq!(fs::read(&fixture.path)?, original);

        fs::write(&fixture.path, b"invalid")?;
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600))?;
        assert_eq!(
            current.logout_local().err(),
            Some(PrivateStoreError::MalformedRecord)
        );
        assert_eq!(fs::read(&fixture.path)?, b"invalid");

        fs::remove_file(&fixture.path)?;
        current.initialize_selected(&CredentialRecord::new(
            fixture.profile.clone(),
            fixture.account.clone(),
            CredentialGeneration::new(u64::MAX)?,
            CredentialStatus::Ready,
            SecretBytes::new(b"current-secret".to_vec()),
        )?)?;
        let original = fs::read(&fixture.path)?;
        assert_eq!(
            current.logout_local().err(),
            Some(PrivateStoreError::GenerationExhausted)
        );
        assert_eq!(fs::read(&fixture.path)?, original);
        Ok(())
    }

    #[test]
    fn local_logout_never_retires_linked_foreign_profile_record()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let slot = CredentialSlotId::new("primary")?;
        let old_profile = SubscriptionIntegrationProfileId::new("previous-profile")?;
        let old =
            PrivateCredentialStore::open_slot(&fixture.path, old_profile.clone(), slot.clone())?;
        old.initialize_selected(&CredentialRecord::new(
            old_profile,
            fixture.account.clone(),
            CredentialGeneration::new(1)?,
            CredentialStatus::Ready,
            SecretBytes::new(b"old-secret".to_vec()),
        )?)?;
        let current =
            PrivateCredentialStore::open_slot(&fixture.path, fixture.profile.clone(), slot)?;
        let displaced = fixture.directory.path().join("displaced-record");
        fs::rename(&fixture.path, &displaced)?;
        let original = fs::read(&displaced)?;

        symlink(&displaced, &fixture.path)?;
        assert_eq!(
            current.logout_local().err(),
            Some(PrivateStoreError::UnsafeRecord)
        );
        assert_eq!(fs::read(&displaced)?, original);
        fs::remove_file(&fixture.path)?;

        fs::hard_link(&displaced, &fixture.path)?;
        assert_eq!(
            current.logout_local().err(),
            Some(PrivateStoreError::UnsafeRecord)
        );
        assert_eq!(fs::read(&displaced)?, original);
        Ok(())
    }

    #[test]
    fn replacement_is_always_a_complete_document() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(1, CredentialStatus::Ready, b"first")?)?;
        let before_inode = fs::metadata(&fixture.path)?.ino();
        store.replace(
            CredentialGeneration::new(1)?,
            &fixture.record(2, CredentialStatus::Ready, b"second")?,
        )?;
        let after_inode = fs::metadata(&fixture.path)?.ino();
        assert_ne!(before_inode, after_inode);
        let loaded = store.load()?.ok_or("missing replacement")?;
        assert_eq!(
            store.acquire_ready(&loaded)?.payload().expose_secret(),
            b"second"
        );
        let leftovers = fs::read_dir(fixture.directory.path())?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
        Ok(())
    }

    #[cfg(feature = "http-refresh")]
    mod http_refresh_tests {
        use std::fs;
        use std::io;
        use std::sync::Arc;
        use std::time::Instant;

        use plakous_http::{
            Bytes, CancellationToken, Client, ExchangeControl, HeaderMap, Limits, Method, Origin,
            Request, StatusCode, TransmissionFact,
        };
        use rcgen::{CertifiedKey, generate_simple_self_signed};
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use rustls::{ClientConfig, RootCertStore, ServerConfig};
        use tempfile::TempDir;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::sync::oneshot;
        use tokio::time::timeout;
        use tokio_rustls::TlsAcceptor;
        use tokio_rustls::server::TlsStream;

        use super::*;

        type ServerError = Box<dyn std::error::Error + Send + Sync>;
        type ServerResult<T = ()> = Result<T, ServerError>;

        struct HttpFixture {
            directory: TempDir,
            path: PathBuf,
            profile: SubscriptionIntegrationProfileId,
            slot: CredentialSlotId,
            account: AccountBinding,
        }

        impl HttpFixture {
            fn new() -> ServerResult<Self> {
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
                PrivateCredentialStore::open_slot(
                    &self.path,
                    self.profile.clone(),
                    self.slot.clone(),
                )
            }

            fn record(
                &self,
                generation: u64,
                status: CredentialStatus,
                payload: &[u8],
            ) -> ServerResult<CredentialRecord> {
                Ok(CredentialRecord::new(
                    self.profile.clone(),
                    self.account.clone(),
                    CredentialGeneration::new(generation)?,
                    status,
                    SecretBytes::new(payload.to_vec()),
                )?)
            }
        }

        struct Endpoint {
            listener: TcpListener,
            acceptor: TlsAcceptor,
            client_config: ClientConfig,
            origin: Origin,
        }

        async fn endpoint() -> ServerResult<Endpoint> {
            let CertifiedKey { cert, signing_key } =
                generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
            let certificate = cert.der().clone();
            let private_key =
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_single_cert(vec![certificate.clone()], private_key)?;
            server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

            let mut roots = RootCertStore::empty();
            roots.add(CertificateDer::from(certificate.to_vec()))?;
            let client_config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            Ok(Endpoint {
                listener,
                acceptor: TlsAcceptor::from(Arc::new(server_config)),
                client_config,
                origin: Origin::parse(&format!("https://{address}"))?,
            })
        }

        fn client(config: ClientConfig, limits: Limits) -> ServerResult<Client> {
            Ok(Client::with_tls_config(limits, config)?)
        }

        fn request(origin: Origin, limits: &Limits, target: &str) -> ServerResult<Request> {
            Ok(Request::new(
                origin,
                Method::POST,
                target,
                HeaderMap::new(),
                Bytes::from_static(b"refresh-body"),
                limits,
            )?)
        }

        fn control() -> ExchangeControl {
            // Loopback TLS exchanges run concurrently with filesystem and
            // subprocess tests. Leave enough absolute-deadline headroom for a
            // loaded test host without changing any production timeout.
            ExchangeControl::new(Instant::now() + Duration::from_secs(30))
        }

        async fn read_request(stream: &mut TlsStream<TcpStream>) -> ServerResult<Vec<u8>> {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1_024];
            let header_end = loop {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "request ended before its head",
                    )
                    .into());
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
                if request.len() > 128 * 1_024 {
                    return Err(io::Error::other("test request head exceeded bound").into());
                }
            };
            let head = std::str::from_utf8(&request[..header_end])?;
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let total = header_end.saturating_add(content_length);
            while request.len() < total {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "request ended before its body",
                    )
                    .into());
                }
                request.extend_from_slice(&buffer[..read]);
            }
            Ok(request)
        }

        fn spawn_response_server(
            listener: TcpListener,
            acceptor: TlsAcceptor,
            response: &'static [u8],
        ) -> tokio::task::JoinHandle<ServerResult<Vec<u8>>> {
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                let captured = read_request(&mut stream).await?;
                stream.write_all(response).await?;
                stream.shutdown().await?;
                Ok(captured)
            })
        }

        async fn read_body(response: &mut ClaimedHttpResponse) -> ServerResult<Vec<u8>> {
            let mut collected = Vec::new();
            while let Some(chunk) = response.next_chunk().await {
                collected.extend_from_slice(&chunk?);
            }
            Ok(collected)
        }

        async fn successful_claimed_response(
            fixture: &HttpFixture,
            store: &PrivateCredentialStore,
            endpoint: Endpoint,
            target: &str,
        ) -> ServerResult<(
            ClaimedHttpResponse,
            tokio::task::JoinHandle<ServerResult<Vec<u8>>>,
        )> {
            successful_claimed_response_with_control(fixture, store, endpoint, target, control())
                .await
        }

        async fn successful_claimed_response_with_control(
            fixture: &HttpFixture,
            store: &PrivateCredentialStore,
            endpoint: Endpoint,
            target: &str,
            control: ExchangeControl,
        ) -> ServerResult<(
            ClaimedHttpResponse,
            tokio::task::JoinHandle<ServerResult<Vec<u8>>>,
        )> {
            let Endpoint {
                listener,
                acceptor,
                client_config,
                origin,
            } = endpoint;
            let server = spawn_response_server(
                listener,
                acceptor,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            );
            store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
            let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
            let limits = Limits::default();
            let client = client(client_config, limits.clone())?;
            let response = store
                .execute_claimed_http(claim, &client, request(origin, &limits, target)?, control)
                .await?;
            Ok((response, server))
        }

        #[tokio::test]
        async fn successful_response_returns_exact_started_claim() -> ServerResult {
            let Endpoint {
                listener,
                acceptor,
                client_config,
                origin,
            } = endpoint().await?;
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                let captured = read_request(&mut stream).await?;
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Test-Secret: response-secret\r\n\r\nok",
                    )
                    .await?;
                stream.shutdown().await?;
                Ok::<_, ServerError>(captured)
            });
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            store.initialize(&fixture.record(
                1,
                CredentialStatus::RefreshRequired,
                b"old-success-secret",
            )?)?;
            let fence = store.load()?.ok_or("missing initial record")?;
            let claim = store.begin_refresh(&fence)?;
            let claimed_fence = claim.fence();
            let limits = Limits::default();
            let client = client(client_config, limits.clone())?;
            let mut response = store
                .execute_claimed_http(
                    claim,
                    &client,
                    request(origin, &limits, "/success?secret=request-secret")?,
                    control(),
                )
                .await?;

            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.refresh_fence(), claimed_fence);
            assert!(!response.body_completed_cleanly());
            let response_debug = format!("{response:?}");
            for secret in [
                "old-success-secret",
                "request-secret",
                "response-secret",
                fixture.path.to_string_lossy().as_ref(),
            ] {
                assert!(!response_debug.contains(secret));
            }

            assert_eq!(read_body(&mut response).await?, b"ok");
            assert!(response.body_completed_cleanly());
            let completed = store.complete_claimed_http(
                response,
                fixture.record(3, CredentialStatus::Ready, b"new-success-secret")?,
            )?;
            assert_eq!(completed.generation(), CredentialGeneration::new(3)?);
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn incomplete_success_response_cannot_complete_refresh() -> ServerResult {
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            let (response, server) =
                successful_claimed_response(&fixture, &store, endpoint().await?, "/incomplete")
                    .await?;
            let unknown = response.refresh_fence();
            assert_eq!(
                store.complete_claimed_http(
                    response,
                    fixture.record(3, CredentialStatus::Ready, b"must-not-install")?,
                ),
                Err(PrivateStoreError::HttpResponseBodyNotComplete)
            );
            assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn body_failure_never_becomes_clean_eof_or_completable() -> ServerResult {
            let Endpoint {
                listener,
                acceptor,
                client_config,
                origin,
            } = endpoint().await?;
            let server = spawn_response_server(
                listener,
                acceptor,
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nx",
            );
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
            let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
            let unknown = claim.fence();
            let limits = Limits::default();
            let client = client(client_config, limits.clone())?;
            let mut response = store
                .execute_claimed_http(
                    claim,
                    &client,
                    request(origin, &limits, "/truncated")?,
                    control(),
                )
                .await?;
            let mut observed_failure = false;
            for _ in 0..3 {
                match response.next_chunk().await {
                    Some(Ok(_)) => {}
                    Some(Err(failure)) => {
                        assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
                        observed_failure = true;
                        break;
                    }
                    None => break,
                }
            }
            assert!(observed_failure, "truncated response did not fail");
            assert_eq!(
                response.transport_failure().map(HttpFailure::phase),
                Some(plakous_http::Phase::ResponseBody)
            );
            assert!(response.next_chunk().await.is_none());
            assert!(!response.body_completed_cleanly());
            assert_eq!(
                store.complete_claimed_http(
                    response,
                    fixture.record(3, CredentialStatus::Ready, b"must-not-install")?,
                ),
                Err(PrivateStoreError::HttpResponseBodyNotComplete)
            );
            assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn cancelled_body_pull_permanently_prevents_completion() -> ServerResult {
            let Endpoint {
                listener,
                acceptor,
                client_config,
                origin,
            } = endpoint().await?;
            let (release_sender, release_receiver) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                let captured = read_request(&mut stream).await?;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                    .await?;
                let _ = release_receiver.await;
                drop(stream);
                Ok::<_, ServerError>(captured)
            });
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
            let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
            let unknown = claim.fence();
            let limits = Limits::default();
            let client = client(client_config, limits.clone())?;
            let mut response = store
                .execute_claimed_http(
                    claim,
                    &client,
                    request(origin, &limits, "/cancel-pull")?,
                    control(),
                )
                .await?;
            {
                let pull = response.next_chunk();
                tokio::pin!(pull);
                tokio::select! {
                    result = &mut pull => {
                        return Err(io::Error::other(format!(
                            "held body pull completed unexpectedly: {result:?}"
                        )).into());
                    }
                    () = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
            assert!(!response.body_completed_cleanly());
            assert_eq!(
                store.complete_claimed_http(
                    response,
                    fixture.record(3, CredentialStatus::Ready, b"must-not-install")?,
                ),
                Err(PrivateStoreError::HttpResponseBodyNotComplete)
            );
            assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
            let _ = release_sender.send(());
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn successful_response_has_consuming_quarantine_terminal() -> ServerResult {
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            let (response, server) =
                successful_claimed_response(&fixture, &store, endpoint().await?, "/quarantine")
                    .await?;
            let unknown = response.refresh_fence();
            assert_eq!(store.quarantine_claimed_http(response)?, unknown);
            assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn retained_response_lock_blocks_mutation_until_terminal_consumption() -> ServerResult
        {
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            let (response, server) =
                successful_claimed_response(&fixture, &store, endpoint().await?, "/held-lock")
                    .await?;
            let unknown = response.refresh_fence();
            let unknown_generation = unknown.generation();
            let contender = fixture.store()?;
            let (sender, receiver) = mpsc::channel();
            let worker = thread::spawn(move || {
                let _ = sender.send(contender.logout(unknown_generation));
            });
            assert!(matches!(
                receiver.recv_timeout(Duration::from_millis(50)),
                Err(RecvTimeoutError::Timeout)
            ));
            assert_eq!(store.quarantine_claimed_http(response)?, unknown);
            assert_eq!(
                receiver.recv_timeout(Duration::from_secs(5))??,
                CredentialGeneration::new(3)?
            );
            worker.join().map_err(|_| "mutation worker panicked")?;
            assert_eq!(
                load_raw_for_test(&store)?
                    .ok_or("missing tombstone")?
                    .status(),
                CredentialStatus::Revoked
            );
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn replaced_lock_inode_cannot_authorize_terminal_write() -> ServerResult {
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            let (mut response, server) =
                successful_claimed_response(&fixture, &store, endpoint().await?, "/replaced-lock")
                    .await?;
            let unknown = response.refresh_fence();
            assert_eq!(read_body(&mut response).await?, b"ok");
            let displaced = fixture.directory.path().join("displaced-http-lock");
            fs::rename(store.lock_path(), &displaced)?;
            create_private_file(&store.lock_path())?;
            assert_eq!(
                store.complete_claimed_http(
                    response,
                    fixture.record(3, CredentialStatus::Ready, b"must-not-install")?,
                ),
                Err(PrivateStoreError::UnsafeLock)
            );
            assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn successful_response_has_consuming_revoke_terminal() -> ServerResult {
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            let (response, server) =
                successful_claimed_response(&fixture, &store, endpoint().await?, "/revoke").await?;
            assert_eq!(
                store.revoke_claimed_http(response)?,
                CredentialGeneration::new(3)?
            );
            let tombstone = load_raw_for_test(&store)?.ok_or("missing tombstone")?;
            assert_eq!(tombstone.generation(), CredentialGeneration::new(3)?);
            assert_eq!(tombstone.status(), CredentialStatus::Revoked);
            assert!(tombstone.payload().is_empty());
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn two_successes_cannot_mix_response_claim_or_store_authority() -> ServerResult {
            let first = HttpFixture::new()?;
            let second = HttpFixture::new()?;
            let first_store = first.store()?;
            let second_store = second.store()?;
            // Both response owners must coexist for the cross-store test. Give
            // the first absolute exchange enough headroom for the second
            // fixture's TLS and durable-store setup under a loaded test host.
            let (mut first_response, first_server) = successful_claimed_response_with_control(
                &first,
                &first_store,
                endpoint().await?,
                "/first",
                ExchangeControl::new(Instant::now() + Duration::from_secs(30)),
            )
            .await?;
            let (mut second_response, second_server) = successful_claimed_response_with_control(
                &second,
                &second_store,
                endpoint().await?,
                "/second",
                ExchangeControl::new(Instant::now() + Duration::from_secs(30)),
            )
            .await?;
            let first_unknown = first_response.refresh_fence();
            let second_unknown = second_response.refresh_fence();
            assert_eq!(read_body(&mut first_response).await?, b"ok");
            assert_eq!(read_body(&mut second_response).await?, b"ok");

            assert_eq!(
                first_store.complete_claimed_http(
                    second_response,
                    first.record(3, CredentialStatus::Ready, b"mixed-second")?,
                ),
                Err(PrivateStoreError::RefreshClaimMismatch)
            );
            assert_eq!(
                second_store.complete_claimed_http(
                    first_response,
                    second.record(3, CredentialStatus::Ready, b"mixed-first")?,
                ),
                Err(PrivateStoreError::RefreshClaimMismatch)
            );
            assert_eq!(
                first_store.load()?.ok_or("first quarantine was lost")?,
                first_unknown
            );
            assert_eq!(
                second_store.load()?.ok_or("second quarantine was lost")?,
                second_unknown
            );
            assert!(String::from_utf8(first_server.await??)?.contains("refresh-body"));
            assert!(String::from_utf8(second_server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn pre_cancelled_execute_never_accepts_and_restores_exact_record() -> ServerResult {
            let endpoint = endpoint().await?;
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            store.initialize(&fixture.record(
                7,
                CredentialStatus::RefreshRequired,
                b"exact-old-payload",
            )?)?;
            let fence = store.load()?.ok_or("missing initial record")?;
            let claim = store.begin_refresh(&fence)?;
            assert_eq!(claim.fence().generation(), CredentialGeneration::new(8)?);
            let limits = Limits::default();
            let client = client(endpoint.client_config, limits.clone())?;
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            let error = store
                .execute_claimed_http(
                    claim,
                    &client,
                    request(endpoint.origin, &limits, "/pre-cancelled")?,
                    ExchangeControl::with_cancellation(
                        Instant::now() + Duration::from_secs(1),
                        cancellation,
                    ),
                )
                .await
                .err()
                .ok_or("pre-cancelled exchange unexpectedly succeeded")?;

            assert_eq!(
                error.disposition(),
                ClaimedHttpDisposition::RestoredNotStarted
            );
            assert_eq!(
                error
                    .transport_failure()
                    .ok_or("missing transport failure")?
                    .transmission(),
                TransmissionFact::NotStarted
            );
            assert_eq!(error.store_error(), None);
            assert!(
                timeout(Duration::from_millis(100), endpoint.listener.accept())
                    .await
                    .is_err()
            );
            let restored = load_raw_for_test(&store)?.ok_or("missing restored record")?;
            assert_eq!(restored.generation(), CredentialGeneration::new(9)?);
            assert_eq!(restored.status(), CredentialStatus::RefreshRequired);
            assert_eq!(restored.payload().expose_secret(), b"exact-old-payload");
            Ok(())
        }

        #[tokio::test]
        async fn post_write_disconnect_is_verified_quarantine() -> ServerResult {
            let Endpoint {
                listener,
                acceptor,
                client_config,
                origin,
            } = endpoint().await?;
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                let captured = read_request(&mut stream).await?;
                drop(stream);
                Ok::<_, ServerError>(captured)
            });
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            store.initialize(&fixture.record(
                1,
                CredentialStatus::Ready,
                b"disconnect-secret",
            )?)?;
            let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
            let claimed_fence = claim.fence();
            let limits = Limits::default();
            let client = client(client_config, limits.clone())?;
            let error = store
                .execute_claimed_http(
                    claim,
                    &client,
                    request(origin, &limits, "/disconnect?token=wire-secret")?,
                    control(),
                )
                .await
                .err()
                .ok_or("disconnected exchange unexpectedly succeeded")?;

            assert_eq!(
                error.disposition(),
                ClaimedHttpDisposition::QuarantinedMayHaveTransmitted
            );
            assert_eq!(
                error
                    .transport_failure()
                    .ok_or("missing transport failure")?
                    .transmission(),
                TransmissionFact::MayHaveTransmitted
            );
            assert_eq!(error.store_error(), None);
            let error_debug = format!("{error:?}");
            let error_display = error.to_string();
            for secret in [
                "disconnect-secret",
                "wire-secret",
                fixture.path.to_string_lossy().as_ref(),
            ] {
                assert!(!error_debug.contains(secret));
                assert!(!error_display.contains(secret));
            }
            let unknown = load_raw_for_test(&store)?.ok_or("missing quarantine")?;
            assert_eq!(unknown.fence(), claimed_fence);
            assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
            assert_eq!(unknown.payload().expose_secret(), b"disconnect-secret");
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn dropping_in_flight_future_after_start_leaves_unknown() -> ServerResult {
            let Endpoint {
                listener,
                acceptor,
                client_config,
                origin,
            } = endpoint().await?;
            let (request_seen_sender, request_seen_receiver) = oneshot::channel();
            let (release_sender, release_receiver) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                let captured = read_request(&mut stream).await?;
                request_seen_sender
                    .send(())
                    .map_err(|()| io::Error::other("request observer dropped"))?;
                let _ = release_receiver.await;
                drop(stream);
                Ok::<_, ServerError>(captured)
            });
            let fixture = HttpFixture::new()?;
            let store = Arc::new(fixture.store()?);
            store.initialize(&fixture.record(
                1,
                CredentialStatus::Ready,
                b"in-flight-secret",
            )?)?;
            let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
            let claimed_fence = claim.fence();
            let limits = Limits::default();
            let client = Arc::new(client(client_config, limits.clone())?);
            let transaction = {
                let store = Arc::clone(&store);
                let client = Arc::clone(&client);
                tokio::spawn(async move {
                    store
                        .execute_claimed_http(
                            claim,
                            &client,
                            request(origin, &limits, "/held")?,
                            control(),
                        )
                        .await
                        .map_err(ServerError::from)
                })
            };

            timeout(Duration::from_secs(2), request_seen_receiver).await??;
            transaction.abort();
            let join_error = transaction
                .await
                .err()
                .ok_or("aborted transaction unexpectedly completed")?;
            assert!(join_error.is_cancelled());
            let unknown = load_raw_for_test(&store)?.ok_or("missing quarantine")?;
            assert_eq!(unknown.fence(), claimed_fence);
            assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
            assert_eq!(unknown.payload().expose_secret(), b"in-flight-secret");
            let _ = release_sender.send(());
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn wrong_store_cannot_clear_unknown() -> ServerResult {
            let endpoint = endpoint().await?;
            let limits = Limits::default();
            let client = client(endpoint.client_config, limits.clone())?;
            let first = HttpFixture::new()?;
            let wrong = HttpFixture::new()?;
            let first_store = first.store()?;
            let wrong_store = wrong.store()?;
            first_store.initialize(&first.record(1, CredentialStatus::Ready, b"first-old")?)?;
            wrong_store.initialize(&wrong.record(1, CredentialStatus::Ready, b"wrong-ready")?)?;
            let first_claim =
                first_store.begin_refresh(&first_store.load()?.ok_or("missing first record")?)?;
            let rejected = wrong_store
                .execute_claimed_http(
                    first_claim,
                    &client,
                    request(endpoint.origin, &limits, "/wrong-store")?,
                    control(),
                )
                .await
                .err()
                .ok_or("wrong-store transaction unexpectedly succeeded")?;
            assert_eq!(
                rejected.disposition(),
                ClaimedHttpDisposition::ClaimRejected
            );
            assert_eq!(
                rejected.store_error(),
                Some(PrivateStoreError::RefreshClaimMismatch)
            );
            assert_eq!(rejected.transport_failure(), None);
            // The rejected error still owns the unstarted claim and therefore
            // the first store's execution lock; release it before the raw
            // (writer-locked) inspection.
            drop(rejected);
            let first_unknown =
                load_raw_for_test(&first_store)?.ok_or("missing first quarantine")?;
            assert_eq!(
                first_unknown.status(),
                CredentialStatus::RefreshOutcomeUnknown
            );
            assert_eq!(first_unknown.payload().expose_secret(), b"first-old");
            let wrong_ready = load_raw_for_test(&wrong_store)?.ok_or("missing wrong record")?;
            assert_eq!(wrong_ready.status(), CredentialStatus::Ready);
            assert_eq!(wrong_ready.payload().expose_secret(), b"wrong-ready");
            assert!(
                timeout(Duration::from_millis(100), endpoint.listener.accept())
                    .await
                    .is_err()
            );
            Ok(())
        }

        #[tokio::test]
        async fn same_path_cross_slot_claim_never_reaches_listener() -> ServerResult {
            let endpoint = endpoint().await?;
            let limits = Limits::default();
            let client = client(endpoint.client_config, limits.clone())?;

            let cross_slot = HttpFixture::new()?;
            let primary = PrivateCredentialStore::open_slot(
                &cross_slot.path,
                cross_slot.profile.clone(),
                CredentialSlotId::new("primary")?,
            )?;
            primary.initialize_selected(&cross_slot.record(
                1,
                CredentialStatus::Ready,
                b"cross-slot-old",
            )?)?;
            let claim = primary.begin_refresh(&primary.load()?.ok_or("missing primary")?)?;
            let secondary = PrivateCredentialStore::open_slot(
                &cross_slot.path,
                cross_slot.profile.clone(),
                CredentialSlotId::new("secondary")?,
            )?;
            let cross_slot_error = secondary
                .execute_claimed_http(
                    claim,
                    &client,
                    request(endpoint.origin, &limits, "/cross-slot")?,
                    control(),
                )
                .await
                .err()
                .ok_or("cross-slot exchange unexpectedly succeeded")?;
            assert_eq!(
                cross_slot_error.disposition(),
                ClaimedHttpDisposition::ClaimRejected
            );
            assert_eq!(
                cross_slot_error.store_error(),
                Some(PrivateStoreError::RefreshClaimMismatch)
            );
            assert_eq!(cross_slot_error.transport_failure(), None);
            drop(cross_slot_error);
            assert_eq!(
                load_raw_for_test(&primary)?
                    .ok_or("missing cross-slot quarantine")?
                    .status(),
                CredentialStatus::RefreshOutcomeUnknown
            );
            assert!(
                timeout(Duration::from_millis(100), endpoint.listener.accept())
                    .await
                    .is_err()
            );
            Ok(())
        }

        #[tokio::test]
        async fn stale_claim_cannot_restore_over_changed_unknown() -> ServerResult {
            let endpoint = endpoint().await?;
            let limits = Limits::default();
            let client = client(endpoint.client_config, limits.clone())?;
            let stale = HttpFixture::new()?;
            let stale_store = stale.store()?;
            stale_store.initialize(&stale.record(1, CredentialStatus::Ready, b"stale-old")?)?;
            let stale_claim =
                stale_store.begin_refresh(&stale_store.load()?.ok_or("missing stale record")?)?;
            // A live claim owns the private directory, so the API cannot
            // replace the record underneath it; inject the change directly to
            // exercise the claim's exact record verification.
            platform::write_record(
                &stale_store,
                &stale.record(
                    3,
                    CredentialStatus::RefreshOutcomeUnknown,
                    b"concurrent-unknown",
                )?,
            )?;
            let rejected = stale_store
                .execute_claimed_http(
                    stale_claim,
                    &client,
                    request(endpoint.origin, &limits, "/stale-claim")?,
                    control(),
                )
                .await
                .err()
                .ok_or("stale transaction unexpectedly succeeded")?;
            assert_eq!(
                rejected.disposition(),
                ClaimedHttpDisposition::ClaimRejected
            );
            assert_eq!(
                rejected.store_error(),
                Some(PrivateStoreError::RefreshClaimMismatch)
            );
            assert_eq!(rejected.transport_failure(), None);
            drop(rejected);
            let still_unknown =
                load_raw_for_test(&stale_store)?.ok_or("missing stale quarantine")?;
            assert_eq!(still_unknown.generation(), CredentialGeneration::new(3)?);
            assert_eq!(
                still_unknown.status(),
                CredentialStatus::RefreshOutcomeUnknown
            );
            assert_eq!(
                still_unknown.payload().expose_secret(),
                b"concurrent-unknown"
            );
            assert!(
                timeout(Duration::from_millis(100), endpoint.listener.accept())
                    .await
                    .is_err()
            );
            Ok(())
        }

        #[tokio::test]
        async fn renamed_store_is_rejected_before_listener_accept() -> ServerResult {
            let endpoint = endpoint().await?;
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            store.initialize(&fixture.record(1, CredentialStatus::Ready, b"rename-old")?)?;
            let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
            let unknown = claim.fence();
            let rotation = RotatedDirectory::replace(fixture.directory.path())?;
            let limits = Limits::default();
            let client = client(endpoint.client_config, limits.clone())?;
            let rejected = store
                .execute_claimed_http(
                    claim,
                    &client,
                    request(endpoint.origin, &limits, "/renamed")?,
                    control(),
                )
                .await
                .err()
                .ok_or("renamed-store exchange unexpectedly succeeded")?;
            assert_eq!(
                rejected.disposition(),
                ClaimedHttpDisposition::ClaimRejected
            );
            assert_eq!(
                rejected.store_error(),
                Some(PrivateStoreError::UnsafeDirectory)
            );
            assert_eq!(rejected.transport_failure(), None);
            assert!(
                timeout(Duration::from_millis(100), endpoint.listener.accept())
                    .await
                    .is_err()
            );
            drop(rotation);
            assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
            Ok(())
        }

        #[tokio::test]
        async fn busy_execution_lock_fails_without_waiting_or_accepting() -> ServerResult {
            // The execution lock is taken by the claim itself, before the
            // durable Unknown write. A store owned by another writer therefore
            // refuses the claim immediately with a local contention fact and
            // leaves the Ready record untouched; nothing reaches the listener.
            let endpoint = endpoint().await?;
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            store.initialize(&fixture.record(1, CredentialStatus::Ready, b"busy-old")?)?;
            let ready = store.load()?.ok_or("missing record")?;
            let blocker = platform::lock(&store)?;
            let started = Instant::now();
            assert_eq!(
                store.begin_refresh(&ready).err(),
                Some(PrivateStoreError::StoreBusy)
            );
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(
                timeout(Duration::from_millis(100), endpoint.listener.accept())
                    .await
                    .is_err()
            );
            drop(blocker);
            let record = load_raw_for_test(&store)?.ok_or("record was lost")?;
            assert_eq!(record.generation(), CredentialGeneration::new(1)?);
            assert_eq!(record.status(), CredentialStatus::Ready);
            assert_eq!(record.payload().expose_secret(), b"busy-old");
            // Once the writer releases the store, the same fence claims normally.
            let claim = store.begin_refresh(&ready)?;
            assert_eq!(claim.fence().generation(), CredentialGeneration::new(2)?);
            let restored = store.restore_refresh_definitely_not_started(claim)?;
            assert_eq!(restored.generation(), CredentialGeneration::new(3)?);
            assert_eq!(restored.status(), CredentialStatus::Ready);
            Ok(())
        }

        #[tokio::test]
        async fn status_read_does_not_block_on_an_in_flight_refresh() -> ServerResult {
            // A claimed exchange owns the execution lock and the directory but
            // not the record lock, so a peer's fence read returns immediately
            // with the durable Unknown fence instead of waiting behind the
            // provider round-trip.
            let fixture = HttpFixture::new()?;
            let store = fixture.store()?;
            let (mut response, server) =
                successful_claimed_response(&fixture, &store, endpoint().await?, "/in-flight")
                    .await?;
            let unknown = response.refresh_fence();
            let reader = fixture.store()?;
            let (sender, receiver) = mpsc::channel();
            let worker = thread::spawn(move || {
                let _ = sender.send(reader.status());
            });
            let observed = receiver.recv_timeout(Duration::from_secs(2))??;
            assert_eq!(observed, Some(CredentialStatus::RefreshOutcomeUnknown));
            let fence = fixture.store()?.load()?.ok_or("fence vanished")?;
            assert_eq!(fence, unknown);
            // A writer still waits for the claim: it must not replace the
            // record under the exchange.
            let writer = fixture.store()?;
            let (write_sender, write_receiver) = mpsc::channel();
            let writer_thread = thread::spawn(move || {
                let _ = write_sender.send(writer.logout(unknown.generation()));
            });
            assert!(matches!(
                write_receiver.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Timeout)
            ));
            assert_eq!(read_body(&mut response).await?, b"ok");
            let completed = store.complete_claimed_http(
                response,
                fixture.record(3, CredentialStatus::Ready, b"new")?,
            )?;
            assert_eq!(completed.generation(), CredentialGeneration::new(3)?);
            // The writer proceeds only after completion and sees the new generation.
            assert_eq!(
                write_receiver.recv_timeout(Duration::from_secs(2))?,
                Err(PrivateStoreError::StaleWriter)
            );
            worker.join().map_err(|_| "status reader panicked")?;
            writer_thread.join().map_err(|_| "writer panicked")?;
            assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
            Ok(())
        }

        #[tokio::test]
        async fn sibling_response_contention_returns_unstarted_claim_without_accepting()
        -> ServerResult {
            let fixture = HttpFixture::new()?;
            let sibling_account = AccountBinding::new("sibling-account")?;
            let sibling_store = PrivateCredentialStore::open_slot(
                fixture.directory.path().join("sibling.json"),
                fixture.profile.clone(),
                fixture.slot.clone(),
            )?;
            let sibling_record = CredentialRecord::new(
                fixture.profile.clone(),
                sibling_account,
                CredentialGeneration::new(1)?,
                CredentialStatus::Ready,
                SecretBytes::new(b"sibling-old".to_vec()),
            )?;
            sibling_store.initialize(&sibling_record)?;
            let sibling_fence = sibling_store.load()?.ok_or("missing sibling record")?;

            let store = fixture.store()?;
            let (held_response, held_server) =
                successful_claimed_response(&fixture, &store, endpoint().await?, "/held-sibling")
                    .await?;
            let Endpoint { listener, .. } = endpoint().await?;
            // The held response owns the private directory. A sibling record in
            // the same directory cannot even claim: the contention surfaces
            // before any durable write, so the sibling stays Ready and never
            // reaches its listener.
            assert_eq!(
                sibling_store.begin_refresh(&sibling_fence).err(),
                Some(PrivateStoreError::StoreBusy)
            );
            assert!(
                timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
            assert_eq!(
                sibling_store.load()?.ok_or("sibling fence was lost")?,
                sibling_fence
            );
            store.quarantine_claimed_http(held_response)?;
            let sibling_claim = sibling_store.begin_refresh(&sibling_fence)?;
            assert_eq!(
                sibling_claim.fence().generation(),
                CredentialGeneration::new(2)?
            );
            let restored = sibling_store.restore_refresh_definitely_not_started(sibling_claim)?;
            assert_eq!(restored.generation(), CredentialGeneration::new(3)?);
            let sibling =
                load_raw_for_test(&sibling_store)?.ok_or("restored sibling record was lost")?;
            assert_eq!(sibling.status(), CredentialStatus::Ready);
            assert_eq!(sibling.payload().expose_secret(), b"sibling-old");
            assert!(String::from_utf8(held_server.await??)?.contains("refresh-body"));
            Ok(())
        }
    }

    fn create_private_file(path: &Path) -> Result<(), PrivateStoreError> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| PrivateStoreError::Io)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| PrivateStoreError::Io)
    }

    fn wait_for_file(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        for _ in 0..400 {
            if path.exists() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
        Err("timed out waiting for helper process".into())
    }

    fn wait_for_release(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        for _ in 0..400 {
            if path.exists() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
        Err("timed out waiting for lock release".into())
    }
}
