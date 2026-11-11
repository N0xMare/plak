use serde::{Deserialize, Serialize};

/// Exact consent scope. A grant never confers general elevated authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorAction {
    AccountKeySet {
        account_id: String,
    },
    AccountKeyClear {
        account_id: String,
    },
    AccountRefresh {
        account_id: String,
    },
    AccountLogout {
        account_id: String,
        local_only: bool,
    },
    AccountRemove {
        account_id: String,
    },
    AccountLoginStart {
        account_id: String,
        notice_revision: String,
        replace_newer: bool,
    },
    ConfigPatch {
        if_match: String,
        keys: Vec<String>,
    },
    TemplatePut {
        id: String,
        if_match: String,
    },
    ProfilePut {
        id: String,
        if_match: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperatorConsent {
    pub action: OperatorAction,
    pub summary: String,
    pub nonce: String,
    pub expires_in_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoginMethod {
    DeviceCode,
    Paste,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoginPhase {
    Starting,
    AwaitingInput,
    Exchanging,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Expired,
}
