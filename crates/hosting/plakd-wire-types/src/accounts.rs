//! Session account choice DTOs (protocol 12, spec 02 §4.4.3). A kind is a
//! provider vertical; a choice is the full account reference the session
//! uses for it. The neutral envelope carries the same shape with a string
//! kind (`plakous_host_protocol::wire::AccountChoice`).

use crate::console::Vertical;
use plakous_host_protocol::wire::KindAccounts;
use serde::{Deserialize, Serialize};

/// One session's account for a provider kind.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountChoice {
    /// Provider kind.
    pub kind: Vertical,
    /// Full account reference of that kind.
    pub account: String,
}

/// Why the host did not apply an account choice. Nothing changed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountChoiceRefusal {
    /// These kinds have no explicit choice, no Ready default slot and not
    /// exactly one Ready account.
    AccountChoiceRequired {
        /// The kinds, each with its accounts and their readiness.
        kinds: Vec<KindAccounts>,
    },
    /// A choice does not name a Ready account of its kind.
    AccountChoiceInvalid {
        kind: String,
        account: String,
        reason: String,
    },
    /// The root is running a turn; a root-kind choice waits for idle.
    RootBusy,
    /// A continue needs the line head's last root turn to have failed.
    NoFailedTurn,
    /// That continue is already queued or submitted.
    AlreadyContinued,
    /// The host could not apply it; the reason is operator-facing.
    Failed { reason: String },
}
