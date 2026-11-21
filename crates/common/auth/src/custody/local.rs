//! The local custody loops around a dialect's snapshot and claimed refresh.

use std::future::Future;
use std::time::Duration;

use super::{
    AccountFacts, CredentialFields, CredentialHold, CustodyError, CustodySlot, CustodyStatus,
    ExpectedFence, IdentityDigest,
};
use crate::{CredentialFence, PrivateStoreError, ReadyCredentialLease};

const SNAPSHOT_ATTEMPTS: usize = 3;
const FENCE_READS: usize = 2;
const SETTLE_WAIT: Duration = Duration::from_secs(30);
const SETTLE_POLL: Duration = Duration::from_millis(100);

/// Facts and their local fence, with the dialect's selection identity if needed.
pub type Snapshot<K> = (AccountFacts, Option<(CredentialFence, K)>);

#[must_use]
pub const fn busy() -> CustodyError {
    CustodyError::Busy {
        retry_after_ms: 100,
    }
}

#[must_use]
pub fn store_error(error: &PrivateStoreError) -> CustodyError {
    if *error == PrivateStoreError::StoreBusy {
        busy()
    } else {
        CustodyError::Store
    }
}

/// Local dialect hooks. Store and claim operations in these hooks must run
/// off the async executor. The dialect owns lifecycle/format refusal, account
/// and selection digests, field parsing, and the claimed grant itself.
pub trait LocalCustody: Sync {
    type Selection: Send + Sync;

    fn snapshot_once<'a>(
        &'a self,
        slot: &'a CustodySlot,
    ) -> impl Future<Output = Result<Snapshot<Self::Selection>, CustodyError>> + Send + 'a;

    /// Whether a snapshot error can mean its reads straddled a replacement.
    fn retry_snapshot(error: &CustodyError) -> bool {
        matches!(error, CustodyError::Store)
    }

    fn profile_active(&self) -> bool;

    fn fields_lease(
        &self,
        fence: CredentialFence,
    ) -> impl Future<Output = Result<Option<ReadyCredentialLease>, CustodyError>> + Send;

    /// Decode only from the lease; the common flow attaches it to the fields.
    fn fields_from_lease(lease: &ReadyCredentialLease) -> Result<CredentialFields, CustodyError>;

    /// Apply the dialect's selection and lifecycle policy and, if eligible,
    /// run the claimed refresh. Report whether the outcome must be read again.
    fn refresh_rejected<'a>(
        &'a self,
        facts: &'a AccountFacts,
        current: &'a (CredentialFence, Self::Selection),
        rejected: &'a ExpectedFence,
        selection: Option<IdentityDigest>,
    ) -> impl Future<Output = Result<bool, CustodyError>> + Send + 'a;
}

async fn snapshot<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
) -> Result<Snapshot<D::Selection>, CustodyError> {
    retry_snapshot(|| dialect.snapshot_once(slot), D::retry_snapshot).await
}

/// Retry a snapshot under a dialect's read policy. Also used to reread
/// after a once-only identity hydration without starting another hydration.
///
/// # Errors
/// Returns the first non-retryable or last retryable refusal.
pub async fn retry_snapshot<T, F: Future<Output = Result<T, CustodyError>>>(
    mut read: impl FnMut() -> F,
    retry: impl Fn(&CustodyError) -> bool,
) -> Result<T, CustodyError> {
    let mut attempts = 1;
    loop {
        match read().await {
            Err(error) if attempts < SNAPSHOT_ATTEMPTS && retry(&error) => attempts += 1,
            result => return result,
        }
    }
}

async fn settled_snapshot<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
) -> Result<Snapshot<D::Selection>, CustodyError> {
    let deadline = tokio::time::Instant::now() + SETTLE_WAIT;
    loop {
        let current = snapshot(dialect, slot).await?;
        if current.0.status != CustodyStatus::Refreshing || tokio::time::Instant::now() >= deadline
        {
            return Ok(current);
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
}

/// Read non-secret custody facts.
///
/// # Errors
/// Returns the dialect's snapshot refusal.
pub async fn facts<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
) -> Result<AccountFacts, CustodyError> {
    Ok(snapshot(dialect, slot).await?.0)
}

/// Inject fields only from the exact Ready lease; recheck the current facts
/// once if the record moves between the facts read and the lease.
///
/// # Errors
/// Returns an exact fence, store, or profile refusal.
pub async fn fields<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
    expected: &ExpectedFence,
) -> Result<CredentialFields, CustodyError> {
    for read in 0..FENCE_READS {
        let (facts, current) = if read == 0 {
            snapshot(dialect, slot).await?
        } else {
            settled_snapshot(dialect, slot).await?
        };
        facts.check_fence(expected)?;
        if !dialect.profile_active() {
            return Err(CustodyError::NotReady {
                facts: Box::new(facts),
            });
        }
        let (fence, _) = current.ok_or(CustodyError::Internal)?;
        if let Some(lease) = dialect.fields_lease(fence.clone()).await?
            && lease.fence() == fence
        {
            return Ok(D::fields_from_lease(&lease)?.holding(CredentialHold::new(lease)));
        }
    }
    Err(busy())
}

/// Settle a peer's claim before judging a rejection, and again after this
/// caller's attempt. A rejection never grants authority over a different account.
///
/// # Errors
/// Returns a snapshot refusal or the dialect's refusal to refresh.
pub async fn after_rejection<D: LocalCustody>(
    dialect: &D,
    slot: &CustodySlot,
    rejected: &ExpectedFence,
    selection: Option<IdentityDigest>,
) -> Result<AccountFacts, CustodyError> {
    let (facts, current) = settled_snapshot(dialect, slot).await?;
    if let Some(current) = &current
        && facts.account_binding == Some(rejected.account_binding)
        && dialect
            .refresh_rejected(&facts, current, rejected, selection)
            .await?
    {
        return Ok(settled_snapshot(dialect, slot).await?.0);
    }
    Ok(facts)
}
