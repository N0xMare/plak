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
    UseMatchingBuild,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredSwitches {
    pub web_accounts_read: bool,
    pub web_config_read: bool,
    pub web_credentials_write: bool,
    pub web_config_write: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConsoleLimits {
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
