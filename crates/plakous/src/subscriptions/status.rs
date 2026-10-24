//! Credential-free subscription and compatibility status values.

/// Credential-free normalized subscription state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NativeAuthStatus {
    /// No selected credential is present.
    SignedOut,
    /// A ready credential generation is present.
    Ready,
    /// A definitely refreshable credential requires refresh before inference.
    RefreshRequired,
    /// A rotating-token outcome is unknown and inference is blocked.
    Quarantined,
    /// A refresh claimed by a live process (this one or another) is in
    /// flight; the outcome settles to another state when its owner finishes.
    Refreshing,
    /// A local revoked tombstone prevents stale resurrection.
    Revoked,
    /// A stored login this build cannot use (made by an earlier build); a new
    /// login is required.
    LoginRequired,
    /// A newer build wrote the stored login, its receipt or its recovery
    /// state. This build neither uses nor rewrites it; see
    /// [`NativeSubscriptions::replace_newer_build_state`](super::NativeSubscriptions::replace_newer_build_state).
    WrittenByNewerBuild,
}

/// Credential-free compatibility acceptance state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompatibilityStatus {
    /// No exact current acceptance receipt is present.
    Required,
    /// The stored receipt matches every current immutable fact.
    Current,
    /// The profile is expired, killed, or not yet valid.
    Inactive,
}

/// Complete credential-free local status for one native integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeSubscriptionStatus {
    pub(super) compatibility: CompatibilityStatus,
    pub(super) credential: NativeAuthStatus,
}

impl NativeSubscriptionStatus {
    /// Returns exact compatibility-receipt state.
    #[must_use]
    pub const fn compatibility(self) -> CompatibilityStatus {
        self.compatibility
    }

    /// Returns normalized local credential state.
    #[must_use]
    pub const fn credential(self) -> NativeAuthStatus {
        self.credential
    }
}

/// Result of one operator-forced refresh grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForcedRefresh {
    pub(super) status: NativeAuthStatus,
    pub(super) access_expires_at: u64,
}

impl ForcedRefresh {
    /// Durable credential state after the grant.
    #[must_use]
    pub const fn status(self) -> NativeAuthStatus {
        self.status
    }

    /// New access-token expiry, Unix seconds.
    #[must_use]
    pub const fn access_expires_at(self) -> u64 {
        self.access_expires_at
    }
}
