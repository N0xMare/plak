use super::{LeaseInstant, NonZeroU64, RunId, SessionRef};

/// An opaque, monotonically fenced branch ownership token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchLease {
    session: SessionRef,
    owner: RunId,
    fence: NonZeroU64,
    expires_at: LeaseInstant,
    lease_seconds: NonZeroU64,
}

impl BranchLease {
    /// Creates a backend-minted lease. Possession alone grants no authority;
    /// mutations authenticate exact backend state, while dispatch admission
    /// checks the store's granted interval using [`SessionStore::lease_now`].
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        session: SessionRef,
        owner: RunId,
        fence: NonZeroU64,
        expires_at: LeaseInstant,
        lease_seconds: NonZeroU64,
    ) -> Self {
        Self {
            session,
            owner,
            fence,
            expires_at,
            lease_seconds,
        }
    }

    /// Returns the owned branch.
    #[must_use]
    pub const fn session(&self) -> SessionRef {
        self.session
    }

    /// Returns the exact run owner.
    #[must_use]
    pub const fn owner(&self) -> RunId {
        self.owner
    }

    /// Returns the monotonically increasing fence.
    #[must_use]
    pub const fn fence(&self) -> NonZeroU64 {
        self.fence
    }

    /// Returns the deadline in the store's lease authority domain.
    #[must_use]
    pub const fn expires_at(&self) -> LeaseInstant {
        self.expires_at
    }

    /// Returns the renewal duration.
    #[must_use]
    pub const fn lease_seconds(&self) -> NonZeroU64 {
        self.lease_seconds
    }
}

/// Request to acquire one branch for an exact run owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcquireBranch {
    session: SessionRef,
    owner: RunId,
    lease_seconds: NonZeroU64,
}

impl AcquireBranch {
    /// Creates an exact acquisition request.
    #[must_use]
    pub const fn new(session: SessionRef, owner: RunId, lease_seconds: NonZeroU64) -> Self {
        Self {
            session,
            owner,
            lease_seconds,
        }
    }

    /// Returns the target branch.
    #[must_use]
    pub const fn session(self) -> SessionRef {
        self.session
    }

    /// Returns the exact run owner.
    #[must_use]
    pub const fn owner(self) -> RunId {
        self.owner
    }

    /// Returns the requested duration.
    #[must_use]
    pub const fn lease_seconds(self) -> NonZeroU64 {
        self.lease_seconds
    }
}
