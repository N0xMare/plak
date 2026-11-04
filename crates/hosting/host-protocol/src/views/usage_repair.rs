//! Typed allowance-read repair facts, independent of product command grammar.

/// Credential condition that prevented an allowance read.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
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
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// An explicitly selected environment key overrides the key file.
    Environment,
    /// The product's key file.
    File,
}

/// One semantic repair for an unavailable allowance read.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum UsageRepair {
    /// Native credential condition, with the account supplied by the vertical.
    Credential(UsageCredentialState),
    /// The provider rejected a key from this source.
    RejectedKey(KeySource),
}
