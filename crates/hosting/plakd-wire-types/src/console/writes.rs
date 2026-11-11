use super::{SecretInput, SettingApplies, SettingValue};
use plakous_host_protocol::topology::Delegation;
use plakous_host_protocol::{CompositionSelection, ProtocolSelection};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigChange {
    pub key: String,
    pub value: SettingValue,
}

/// Typed surgical changes, never a TOML upload. Omitted fields are untouched.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "field", rename_all = "snake_case", deny_unknown_fields)]
pub enum TemplateChange {
    Account {
        value: String,
    },
    Model {
        value: String,
    },
    Effort {
        value: Option<String>,
    },
    FastMode {
        value: bool,
    },
    Composition {
        value: Option<CompositionSelection>,
    },
    Protocol {
        value: Option<ProtocolSelection>,
    },
    Description {
        value: String,
    },
    Access {
        value: TemplateAccess,
    },
    Tools {
        value: Vec<String>,
    },
    Spawns {
        value: Vec<String>,
    },
    MayAdopt {
        value: bool,
    },
    Delegation {
        value: Delegation,
    },
    CostTier {
        value: Option<u8>,
    },
    MaxTurns {
        value: Option<u32>,
    },
    ShowModel {
        value: bool,
    },
    OnMint {
        value: Option<SecretInput>,
    },
    /// One exact environment name; null removes it, omission preserves it.
    Env {
        name: String,
        value: Option<SecretInput>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TemplateAccess {
    Full,
    ReadExecute,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedField {
    pub key: String,
    pub applies: SettingApplies,
}
