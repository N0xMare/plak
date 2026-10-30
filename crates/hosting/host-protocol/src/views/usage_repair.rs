//! Typed allowance-read repair facts, independent of product command grammar.

/// Credential condition that prevented an allowance read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCredentialState {
    /// A token refresh is required.
    RefreshRequired,
    /// A rotating-token refresh outcome is unknown.
    Quarantined,
    /// The credential is revoked.
    Revoked,
    /// An older build's login must be replaced.
    LoginRequired,
    /// A newer build wrote the login.
    WrittenByNewerBuild,
}

/// Which key source the operator must repair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeySource {
    /// An explicitly selected environment key overrides the key file.
    Environment,
    /// The product's key file.
    File,
}

/// One semantic repair for an unavailable allowance read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageRepair {
    /// Native credential condition, with the account supplied by the vertical.
    Credential(UsageCredentialState),
    /// The provider rejected a key from this source.
    RejectedKey(KeySource),
}
