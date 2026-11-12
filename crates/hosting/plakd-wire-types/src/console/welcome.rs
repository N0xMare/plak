use super::{nullable, required};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostWelcome {
    pub console: ConsoleWelcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConsoleWelcome {
    pub face: ConsoleFace,
    pub accounts_read: CapabilityState,
    pub config_read: CapabilityState,
    pub credentials_write: CapabilityState,
    pub config_write: CapabilityState,
    pub files_read: CapabilityState,
    pub terminal: CapabilityState,
    pub terminal_limits: super::TerminalLimits,
    pub workspace_limits: WorkspaceReadLimits,
    pub configured_switches: ConfiguredSwitches,
    pub limits: ConsoleLimits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleFace {
    Socket,
    Web,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilityState {
    pub enabled: bool,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<DisabledReason>")]
    pub disabled_reason: Option<DisabledReason>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<CapabilityRemedy>")]
    pub remedy: Option<CapabilityRemedy>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DisabledReason {
    Configuration,
    NotCompiled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRemedy {
    EnableWebAccountsReadAndRestart,
    EnableWebConfigReadAndRestart,
    EnableWebCredentialsWriteAndRestart,
    EnableWebConfigWriteAndRestart,
    EnableWebFilesReadAndRestart,
    EnableWebTerminalAndRestart,
    UseMatchingBuild,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Independent endpoint capabilities have a flat wire representation."
)]
pub struct ConfiguredSwitches {
    pub web_accounts_read: bool,
    pub web_config_read: bool,
    pub web_credentials_write: bool,
    pub web_config_write: bool,
    pub web_files_read: bool,
    pub web_terminal: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConsoleLimits {
    pub confirmation_ttl_ms: u64,
    pub confirmations_per_principal: u64,
    pub credential_mutations_per_minute: u64,
    pub config_mutations_per_minute: u64,
    pub login_starts_per_minute: u64,
    pub pending_logins_per_principal: u64,
    pub pending_logins_host: u64,
    pub login_lifetime_ms: u64,
    pub login_poll_ms: u64,
    pub secret_input_bytes: u64,
    pub credential_request_bytes: u64,
    pub max_request_bytes: u64,
    pub max_response_bytes: u64,
    pub max_definition_bytes: u64,
    pub page_rows: u64,
    pub dependency_bytes: u64,
    pub dependency_definitions: u64,
    pub message_rate_per_second: u32,
    pub message_burst: u32,
    pub inflight_per_connection: u64,
    pub inflight_per_principal: u64,
    pub inflight_host: u64,
    pub provider_reads_inflight_host: u64,
    pub provider_deadline_ms: u64,
    pub request_deadline_ms: u64,
    pub usage_cache_ms: u64,
    pub usage_reread_ms: u64,
    pub cursor_ttl_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceReadLimits {
    pub page_bytes: u64,
    pub file_bytes: u64,
    pub reads_inflight_per_principal: u64,
    pub requests_per_second: u64,
    pub request_burst: u64,
    pub output_bytes_per_second: u64,
    pub output_burst_bytes: u64,
    pub hash_bytes_per_second: u64,
    pub hash_burst_bytes: u64,
    pub inflight_host: u64,
    pub search_starts_per_minute: u64,
    pub search_inflight_per_principal: u64,
    pub file_deadline_ms: u64,
    pub changes_deadline_ms: u64,
    pub changes_entries: u64,
    pub changes_targets_per_principal: u64,
    pub changes_token_ttl_ms: u64,
}
