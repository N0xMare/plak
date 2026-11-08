//! Temporary serialized reload values; probe and file authority stays with the host.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The installed `plak` a reload would start, and the state key it answered;
/// it crosses from the session's host to the face that asked (spec 02 §5.1).
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReloadTarget {
    pub executable: PathBuf,
    pub capacity: Option<Box<CarryCapacityProbe>>,
    pub capacity_error: Option<String>,
    /// `None` when the build did not answer in time or with one key.
    pub state_key: Option<String>,
    pub probe: Option<ReloadProbe>,
    /// The destination's exact-reopen verdict for a same-key reload; `None`
    /// when it was not asked, did not answer in time, or predates the question.
    pub reopen: Option<Box<ReopenProbe>>,
    /// The session's own reload request this reload performs, if any.
    pub request: Option<Box<ReloadRequest>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CarryCapacityProbe {
    pub route: String,
    pub total_window: Option<u64>,
    pub admitted_input: Option<u64>,
    pub estimated_tokens: u64,
    pub messages: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, schemars::JsonSchema)]
pub struct ReloadProbe {
    pub state_key: String,
    pub export_versions: Vec<u32>,
    pub config_error: Option<String>,
    /// The answering build's binary id, when its install manifest or the
    /// home's identity cache already names it; absent when it is not known
    /// without hashing, and from builds that predate it.
    #[serde(default)]
    pub binary_id: Option<String>,
}

/// The destination's answer to "would exact reopen of this session attach?",
/// asked by `/reload` before teardown (hidden `version --reopen-probe <id>`).
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, schemars::JsonSchema)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ReopenProbe {
    /// Exact reopen attaches.
    Opens,
    /// Exact reopen refuses because this build derives a different frozen
    /// binding input, named by `change`.
    BindingChanged { change: String },
    /// Exact reopen refuses for another reason (login, configuration,
    /// unsettled work, storage); never a carry trigger.
    Refused { reason: String },
    /// The destination could not answer without renewing a credential
    /// (`needs_refresh`); the source treats it as no verdict.
    Unknown { reason: String },
}

/// One request, as a requester writes it and the host reads it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ReloadRequest {
    #[serde(default)]
    pub version: u32,
    pub request_id: String,
    pub session_id: String,
    /// The installed build the requester expects the session to reload onto.
    pub expect_binary: String,
    /// The announcing host instance the request is for.
    #[serde(default)]
    pub nonce: String,
}
