#![allow(
    unused_qualifications,
    reason = "schemars qualifies nullable schema helpers in generated code"
)]

use plakous_host_protocol::views::UsageRepair;
use plakous_host_protocol::views::presentation::{SessionLedgerView, UsageVertical};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::console::{
    Diagnostic, Page, ReadRemedy, RecordedUsage, UsageWindow, nullable, required,
};

use super::shared::SharedHostResponse;
use super::{ProfileDefinition, ProfileSummary, TemplateDefinition, TemplateSummary};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum HostResponse {
    Draft(HostResponseV13),
    Shared(Box<SharedHostResponse>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "response", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostResponseV13 {
    ProfilesList {
        etag: String,
        #[serde(deserialize_with = "required")]
        #[schemars(required, schema_with = "nullable::<String>")]
        current: Option<String>,
        rows: Vec<ProfileSummary>,
        diagnostics: Vec<Diagnostic>,
        page: Page,
    },
    ProfilesGet {
        profile: Box<ProfileDefinition>,
    },
    TemplatesList {
        etag: String,
        rows: Vec<TemplateSummary>,
        diagnostics: Vec<Diagnostic>,
        page: Page,
    },
    TemplatesGet {
        template: Box<TemplateDefinition>,
    },
    AccountsUsage {
        as_of: String,
        window: UsageWindow,
        from: String,
        to: String,
        rows: Vec<AccountUsage>,
        unattributed: Box<RecordedUsage>,
        diagnostics: Vec<Diagnostic>,
        page: Page,
    },
    Refused {
        reason: ReadRefusal,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ReadRefusal {
    Draft(ChangesRefusal),
    Shared(crate::console::ReadRefusal),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChangesRefusal {
    VcsUnsupportedVersion { found: String, required: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountUsage {
    pub account_id: String,
    pub recorded: RecordedUsage,
    pub allowance: Allowance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Allowance {
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub observed_at: Option<String>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<u64>")]
    pub stale_age_ms: Option<u64>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<VerticalUsageView>")]
    pub view: Option<VerticalUsageView>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<Diagnostic>")]
    pub unavailable: Option<Diagnostic>,
    pub remedies: Vec<ReadRemedy>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageLimitsView {
    pub verticals: Vec<VerticalUsageView>,
    pub session: SessionLedgerView,
    pub stale_age_seconds: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerticalUsageView {
    pub repair: Option<UsageRepair>,
    pub vertical: UsageVertical,
    pub account_email: Option<String>,
    pub plan_label: Option<String>,
    pub observed_at: Option<String>,
    pub state: VerticalUsageState,
    pub balances: Vec<Money>,
}

/// Amounts in integer micro-units of the reported currency. Missing means
/// unreported; zero stays a measurement. The display string is not parsed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Money {
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_micros: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_micros: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_micros: Option<i64>,
    pub display: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum VerticalUsageState {
    Ready { meters: Vec<UsageMeterView> },
    NotConnected,
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageMeterView {
    pub label: String,
    pub group: Option<String>,
    pub reading: UsageMeterReading,
    pub resets_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum UsageMeterReading {
    Percent {
        floored_percent: u8,
        exact_percent: String,
        percent_left: Option<u8>,
    },
    Amounts {
        entries: Vec<Money>,
    },
    Unreported,
}
