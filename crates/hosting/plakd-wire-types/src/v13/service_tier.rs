#![allow(
    unused_qualifications,
    reason = "schemars qualifies nullable schema helpers in generated code"
)]

use plakous_host_protocol::{
    CompositionSelection, ProtocolSelection,
    topology::{Delegation, Tier},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::console::{
    Diagnostic, ProfileBody, ProfileKind, ProtectedEnvironment, ProtectedText, TemplateSource,
    nullable, required,
};

use super::shared::{SharedHostRequest, SharedSurfaceObservation, SharedTemplateChange};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    Fast,
    Ultrafast,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum HostRequest {
    Draft(HostRequestV13),
    Shared(Box<SharedHostRequest>),
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "request", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostRequestV13 {
    TemplatePut {
        id: String,
        changes: Vec<TemplateChange>,
        if_match: String,
        confirmation: String,
    },
}

impl std::fmt::Debug for HostRequestV13 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TemplatePut").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TemplateChange {
    Draft(TemplateChangeV13),
    Shared(SharedTemplateChange),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "field", rename_all = "snake_case", deny_unknown_fields)]
pub enum TemplateChangeV13 {
    ServiceTier {
        #[serde(deserialize_with = "required")]
        #[schemars(required, schema_with = "nullable::<ServiceTier>")]
        value: Option<ServiceTier>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SurfaceObservation {
    Draft(SurfaceObservationV13),
    Shared(Box<SharedSurfaceObservation>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceObservationV13 {
    Profiles(ProfilesView),
    SessionProfile(Box<ProfileRow>),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfilesView {
    pub rows: Vec<ProfileRow>,
    pub current: Option<String>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRow {
    pub id: String,
    pub differences: Vec<ProfileDifference>,
    pub aliases: Vec<String>,
    pub root: ProfileRole,
    pub cards: Vec<ProfileRole>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_kinds: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRole {
    pub label: String,
    pub account: String,
    pub model: String,
    pub effort: String,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<ServiceTier>")]
    pub service_tier: Option<ServiceTier>,
    pub depth: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProfileDifference {
    RootAccount {
        session: String,
        profile: String,
    },
    RootModel {
        session: String,
        profile: String,
    },
    RootEffort {
        session: String,
        profile: String,
    },
    RootServiceTier {
        #[serde(deserialize_with = "required")]
        #[schemars(required, schema_with = "nullable::<ServiceTier>")]
        session: Option<ServiceTier>,
    },
    WorkingSet,
    Card(String),
    Roster,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileSummary {
    pub id: String,
    pub kind: ProfileKind,
    pub summary: ProfileRow,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileDefinition {
    pub id: String,
    pub kind: ProfileKind,
    pub etag: String,
    pub body: ProfileBody,
    pub summary: ProfileRow,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateRow {
    pub name: String,
    pub account: String,
    pub model: String,
    pub effort: Option<String>,
    pub composition: String,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<ServiceTier>")]
    pub service_tier: Option<ServiceTier>,
    pub unready_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateSummary {
    pub id: String,
    pub source: TemplateSource,
    pub summary: TemplateRow,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateDefinition {
    pub id: String,
    pub source: TemplateSource,
    pub etag: String,
    pub body: TemplateBody,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateBody {
    pub account: String,
    pub model: String,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub effort: Option<String>,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<ServiceTier>")]
    pub service_tier: Option<ServiceTier>,
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
