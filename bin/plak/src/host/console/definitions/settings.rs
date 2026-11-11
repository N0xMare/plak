//! The console allowlist projects the product registry, not a client schema.

use crate::product_config::{
    CONFIG_KEYS, ConfigField, ConfigKey, ConfigValueOrigin, ProductConfig,
};
use plakd_wire_types::console::{
    ConfigFieldValue, FieldOrigin, HomeScope, SettingApplies, SettingChoice, SettingKind,
    SettingRow, SettingValue,
};

pub(super) fn keys() -> impl Iterator<Item = &'static ConfigKey> {
    CONFIG_KEYS.iter().filter(|key| {
        matches!(
            key.field,
            ConfigField::RootAgent
                | ConfigField::RootComposition
                | ConfigField::WorkingSet
                | ConfigField::Subagents
                | ConfigField::MaxDepth
                | ConfigField::MaxChildren
                | ConfigField::MaxNodes
                | ConfigField::WorkspaceIdleSeconds
                | ConfigField::WorkspaceBudgetMib
                | ConfigField::ChildRetentionDays
                | ConfigField::ChildRetentionCount
                | ConfigField::MaxAttempts
                | ConfigField::TransientResumes
                | ConfigField::RequestsPerAccount
                | ConfigField::RequireReviewEvidence
                | ConfigField::AutoContinue
                | ConfigField::MaxModelRounds
                | ConfigField::JobsForegroundSeconds
                | ConfigField::JobsMaxRuntimeHours
                | ConfigField::ApiFullWindow
                | ConfigField::CompactionEnabled
                | ConfigField::SkillsEnabled
                | ConfigField::TelemetryMaxBytes
                | ConfigField::TelemetryMaxAgeDays
                | ConfigField::ResumeAfterLoss
        )
    })
}

pub(super) fn fields(config: &ProductConfig) -> Vec<ConfigFieldValue> {
    keys()
        .map(|key| ConfigFieldValue {
            key: key.path.into(),
            value: value(config, key.field),
            origin: origin(config, key.field),
        })
        .collect()
}

pub(super) fn rows(config: &ProductConfig) -> Vec<SettingRow> {
    let defaults = ProductConfig::default_product();
    keys()
        .map(|key| SettingRow {
            key: key.path.into(),
            label: key.path.into(),
            description: key.doc.into(),
            kind: kind(key),
            value: value(config, key.field),
            default: value(&defaults, key.field),
            origin: origin(config, key.field),
            applies: applies(key),
            scope: HomeScope::Home,
            read_only: true,
        })
        .collect()
}

fn origin(config: &ProductConfig, field: ConfigField) -> FieldOrigin {
    match config.origin(field) {
        ConfigValueOrigin::Default => FieldOrigin::Default,
        ConfigValueOrigin::User | ConfigValueOrigin::Project => FieldOrigin::Home,
    }
}

fn value(config: &ProductConfig, field: ConfigField) -> SettingValue {
    match config.console_field(field) {
        Some(toml_edit::Value::Boolean(value)) => SettingValue::Bool(*value.value()),
        Some(toml_edit::Value::Integer(value)) => SettingValue::Integer(*value.value()),
        Some(toml_edit::Value::String(value)) => SettingValue::String(value.into_value()),
        Some(toml_edit::Value::Array(value)) => SettingValue::Strings(
            value
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect(),
        ),
        _ => SettingValue::Null,
    }
}

fn kind(key: &ConfigKey) -> SettingKind {
    match key.kind {
        "bool" => SettingKind::Bool,
        "array of string" => SettingKind::StringList,
        "integer" => {
            let (min, max) = key
                .constraint
                .split_once("..=")
                .and_then(|(min, max)| {
                    Some((
                        min.parse().ok()?,
                        max.split_whitespace().next()?.parse().ok()?,
                    ))
                })
                .unwrap_or((1, i64::MAX));
            SettingKind::Integer { min, max }
        }
        "enum" => SettingKind::Enum {
            choices: key
                .constraint
                .split(';')
                .next()
                .unwrap_or_default()
                .split('|')
                .map(|value| SettingChoice {
                    value: value.into(),
                    label: value.into(),
                    description: String::new(),
                })
                .collect(),
        },
        _ => SettingKind::String,
    }
}

pub(super) fn applies(key: &ConfigKey) -> SettingApplies {
    match key.field {
        ConfigField::AutoContinue => SettingApplies::Immediate,
        ConfigField::MaxModelRounds
        | ConfigField::ResumeAfterLoss
        | ConfigField::RequestsPerAccount
        | ConfigField::CompactionEnabled => SettingApplies::NextAttach,
        ConfigField::TelemetryMaxBytes | ConfigField::TelemetryMaxAgeDays => {
            SettingApplies::HostRestart
        }
        _ => SettingApplies::NewSession,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_exposed_key_matches_the_documented_host_applicability()
    -> Result<(), Box<dyn std::error::Error>> {
        let spec = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/spec/07-config-reference.md"),
        )?;
        let rows = rows(&ProductConfig::default_product());
        assert_eq!(rows.len(), 25);
        for row in rows {
            let applies = serde_json::to_value(row.applies)?;
            let label = applies.as_str().ok_or("applicability spelling")?;
            assert!(
                spec.lines()
                    .any(|line| line.starts_with(&format!("| `{}` | `{label}` |", row.key))),
                "{} must have exactly this documented host applicability",
                row.key
            );
            assert!(row.read_only);
            let registry = crate::product_config::config_key(&row.key).ok_or("registry key")?;
            assert_eq!(row.description, registry.doc);
        }
        Ok(())
    }

    #[test]
    fn integer_metadata_bounds_match_the_product_parser() -> Result<(), Box<dyn std::error::Error>>
    {
        for row in rows(&ProductConfig::default_product()) {
            let SettingKind::Integer { min, max } = row.kind else {
                continue;
            };
            let mut config = ProductConfig::default_product();
            config.set_key(&row.key, &min.to_string())?;
            config.set_key(&row.key, &max.to_string())?;
            assert!(
                config.set_key(&row.key, &(min - 1).to_string()).is_err(),
                "{} minimum",
                row.key
            );
            let over = i128::from(max) + 1;
            assert!(
                config.set_key(&row.key, &over.to_string()).is_err(),
                "{} maximum",
                row.key
            );
        }
        Ok(())
    }
}
