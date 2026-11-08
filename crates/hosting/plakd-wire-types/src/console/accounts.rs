use super::{Diagnostic, ReadRemedy, nullable, required};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountStatus {
    pub account_id: String,
    pub vertical: Vertical,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub name: Option<String>,
    pub label: String,
    pub compiled: bool,
    pub observed_at: String,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<NativeStatus>")]
    pub native: Option<NativeStatus>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<KeyStatus>")]
    pub key: Option<KeyStatus>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<Vec<String>>")]
    pub same_upstream_as: Option<Vec<String>>,
    pub remedies: Vec<ReadRemedy>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Vertical {
    Claude,
    Codex,
    Grok,
    Anthropic,
    Openai,
    #[serde(rename = "xai-api")]
    XaiApi,
    Openrouter,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeStatus {
    pub compatibility: Compatibility,
    pub credential: Credential,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<ProfileNotice>")]
    pub profile: Option<ProfileNotice>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<NativeIdentity>")]
    pub identity: Option<NativeIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Compatibility {
    Current,
    Required,
    Inactive,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    SignedOut,
    Ready,
    RefreshRequired,
    Refreshing,
    Quarantined,
    LoginRequired,
    ProfileChanged,
    WrittenByNewerBuild,
    Revoked,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileNotice {
    pub id: String,
    pub notice_revision: String,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub reviewed_at: Option<String>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeIdentity {
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub email: Option<String>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub plan: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyStatus {
    pub source: KeySource,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub fingerprint: Option<String>,
    pub environment_override: bool,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub shadowed_file_fingerprint: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    Environment,
    File,
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum UsageWindow {
    #[serde(rename = "24h")]
    Hours24,
    #[serde(rename = "7d")]
    Days7,
    #[serde(rename = "30d")]
    Days30,
}

impl UsageWindow {
    #[must_use]
    pub const fn seconds(self) -> u64 {
        match self {
            Self::Hours24 => 86_400,
            Self::Days7 => 604_800,
            Self::Days30 => 2_592_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountUsage {
    pub account_id: String,
    pub recorded: RecordedUsage,
    pub allowance: Allowance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Allowance {
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub observed_at: Option<String>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<u64>")]
    pub stale_age_ms: Option<u64>,
    #[serde(deserialize_with = "required")]
    #[schemars(
        required,
        schema_with = "nullable::<plakous_host_protocol::views::presentation::VerticalUsageView>"
    )]
    pub view: Option<plakous_host_protocol::views::presentation::VerticalUsageView>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<Diagnostic>")]
    pub unavailable: Option<Diagnostic>,
    pub remedies: Vec<ReadRemedy>,
}

/// Non-null values are measured retained subtotals, not whole-window totals.
/// Null means no known measurement, including an unproved empty window.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordedUsage {
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<Decimal>")]
    pub input_tokens: Option<Decimal>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<Decimal>")]
    pub output_tokens: Option<Decimal>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<Decimal>")]
    pub reasoning_tokens: Option<Decimal>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<Decimal>")]
    pub cached_input_tokens: Option<Decimal>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<Decimal>")]
    pub cache_write_tokens: Option<Decimal>,
    /// Sum of terminal `model_operations`, not turns, tools or transport retries.
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<Decimal>")]
    pub calls: Option<Decimal>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<ReportedCost>")]
    pub cost: Option<ReportedCost>,
    pub coverage: Coverage,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportedCost {
    pub micros: Decimal,
    pub currency: Currency,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum Currency {
    #[serde(rename = "USD")]
    Usd,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub from: Option<String>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub to: Option<String>,
    /// False whenever whole-home producer coverage is unproved. Retained
    /// subtotals remain meaningful; an apparently empty store is not proof.
    pub complete: bool,
    pub reasons: Vec<CoverageReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoverageReason {
    TelemetryDisabled,
    StoreUnavailable,
    PreRecording,
    Retention,
    WriterGap,
    MissingMeasurements,
    UnknownAttribution,
    LegacySchema,
}

/// Canonical nonnegative integer string; never a floating-point JSON number.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Decimal(String);

impl Decimal {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<u128> for Decimal {
    fn from(value: u128) -> Self {
        Self(value.to_string())
    }
}
impl From<Decimal> for String {
    fn from(value: Decimal) -> Self {
        value.0
    }
}
impl TryFrom<String> for Decimal {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.parse::<u128>().is_ok()
            && (value == "0"
                || (!value.starts_with('0') && value.bytes().all(|byte| byte.is_ascii_digit())))
        {
            Ok(Self(value))
        } else {
            Err("invalid nonnegative decimal")
        }
    }
}
impl schemars::JsonSchema for Decimal {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Decimal".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = <String as schemars::JsonSchema>::json_schema(generator);
        schema.insert("pattern".into(), "^(0|[1-9][0-9]*)$".into());
        schema.insert("maxLength".into(), 39.into());
        schema
    }
}
