//! Session account choice on the framed wire (protocol 12).
//!
//! A kind is the provider id a template names. A choice is the full account
//! reference the session uses for that kind. This module does not decide
//! which account is ready; it only carries the choice.

use serde::{Deserialize, Serialize};

/// One session's account for a provider kind.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountChoice {
    /// Provider kind id. A named account is not a kind.
    pub kind: String,
    /// Full account reference for that kind.
    pub account: String,
}

/// Accounts of one kind a picker may offer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KindAccounts {
    /// Provider kind id.
    pub kind: String,
    /// Known accounts of that kind, ready and not.
    pub rows: Vec<AccountOption>,
}

/// One account a picker row can name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountOption {
    /// Full account reference.
    pub account: String,
    /// The credential is ready for a new dispatch.
    pub ready: bool,
    /// Operator-facing label, when the host has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
