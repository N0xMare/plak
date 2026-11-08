use super::{Diagnostic, Page, nullable, required};
use plakous_host_protocol::{
    CompositionSelection, ProtocolSelection,
    topology::{Delegation, Tier},
    views::{ModelEntry, ProfileRow, TemplateRow},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Builtin,
    User,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileSummary {
    pub id: String,
    pub kind: ProfileKind,
    pub summary: ProfileRow,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileBody {
    pub root: String,
    pub working_set: Vec<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileDefinition {
    pub id: String,
    pub kind: ProfileKind,
    pub etag: String,
    pub body: ProfileBody,
    pub summary: ProfileRow,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TemplateSource {
    Builtin,
    Home,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateSummary {
    pub id: String,
    pub source: TemplateSource,
    pub summary: TemplateRow,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateDefinition {
    pub id: String,
    pub source: TemplateSource,
    pub etag: String,
    pub body: TemplateBody,
    pub diagnostics: Vec<Diagnostic>,
}

/// The read contract deliberately withholds literals instead of round-tripping secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProtectedText {
    Absent {},
    Withheld {},
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProtectedEnvironment {
    pub name: String,
    pub value: ProtectedText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateBody {
    pub account: String,
    pub model: String,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub effort: Option<String>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub extra: Option<String>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<CompositionSelection>")]
    pub composition: Option<CompositionSelection>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<ProtocolSelection>")]
    pub protocol: Option<ProtocolSelection>,
    pub description: String,
    pub max_tier: Tier,
    pub tools: Vec<String>,
    pub spawns: Vec<String>,
    pub may_adopt: bool,
    pub delegation: Delegation,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<u8>")]
    pub cost_tier: Option<u8>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<u32>")]
    pub max_turns: Option<u32>,
    pub show_model: bool,
    pub on_mint: ProtectedText,
    pub env: Vec<ProtectedEnvironment>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum SettingValue {
    Bool(bool),
    Integer(i64),
    String(String),
    Strings(Vec<String>),
    Null,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldOrigin {
    Home,
    Default,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HomeScope {
    Home,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettingApplies {
    Immediate,
    NextAttach,
    NewSession,
    HostRestart,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigFieldValue {
    pub key: String,
    pub value: SettingValue,
    pub origin: FieldOrigin,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingKind {
    Bool,
    Integer { min: i64, max: i64 },
    Enum { choices: Vec<SettingChoice> },
    String,
    StringList,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettingChoice {
    pub value: String,
    pub label: String,
    pub description: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettingRow {
    pub key: String,
    pub label: String,
    pub description: String,
    pub kind: SettingKind,
    pub value: SettingValue,
    pub default: SettingValue,
    pub origin: FieldOrigin,
    pub applies: SettingApplies,
    pub scope: HomeScope,
    pub read_only: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    Bundled,
    Cached,
    Unavailable,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogPage {
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub account_id: Option<String>,
    pub source: CatalogSource,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub observed_at: Option<String>,
    pub rows: Vec<ModelEntry>,
    pub diagnostics: Vec<Diagnostic>,
    pub page: Page,
}
