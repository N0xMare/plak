//! Exact row contract checks, separate from provider-authoritative capacity facts.

use super::{
    CatalogInputModality, CatalogModelWire, CatalogShellType, CodexCatalogToolMode,
    release_enrolled_codex_tool_mode,
};
use plakous_auth::CompatibilityDrift;

fn version(value: &str) -> Option<[u64; 3]> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some([
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ])
}

pub(in crate::native_auth) fn contract_drift(
    model: &CatalogModelWire,
) -> Option<CompatibilityDrift> {
    let pinned = crate::OPENAI_CODEX_REFERENCE_CLIENT_VERSION;
    if let Some(required) = &model.minimal_client_version {
        match (version(required), version(pinned)) {
            (Some(required_version), Some(pinned_version)) if required_version > pinned_version => {
                return Some(CompatibilityDrift::ClientVersion {
                    required: required.clone(),
                    pinned,
                });
            }
            (None, _) | (_, None) => {
                return Some(field(
                    model,
                    "minimal_client_version",
                    "invalid",
                    "semantic version",
                ));
            }
            _ => {}
        }
    }
    if model.visibility.recognized().is_none() {
        return Some(field(
            model,
            "visibility",
            "unrecognized",
            "list, hide or none",
        ));
    }
    let mode = model.tool_mode.recognized();
    let expected = release_enrolled_codex_tool_mode(&model.slug)?;
    let accepted = match expected {
        CodexCatalogToolMode::CodeModeOnly => matches!(
            mode,
            Some(CodexCatalogToolMode::CodeModeOnly | CodexCatalogToolMode::Direct)
        ),
        _ => mode == Some(expected),
    };
    if !accepted {
        return Some(field(
            model,
            "tool_mode",
            "unsupported",
            "enrolled tool mode",
        ));
    }
    if !model.input_modalities.contains(&CatalogInputModality::Text) {
        return Some(field(model, "input_modalities", "no text", "text input"));
    }
    if !matches!(
        model.shell_type,
        CatalogShellType::Default
            | CatalogShellType::Local
            | CatalogShellType::UnifiedExec
            | CatalogShellType::ShellCommand
    ) {
        return Some(field(model, "shell_type", "unsupported", "local tools"));
    }
    if !super::CodexReasoningEffortSet::from_catalog(&model.supported_reasoning_levels)
        .supports_named_reasoning()
    {
        return Some(field(
            model,
            "supported_reasoning_levels",
            "no named reasoning",
            "named reasoning",
        ));
    }
    None
}

fn field(
    model: &CatalogModelWire,
    field: &'static str,
    served: &str,
    pinned: &'static str,
) -> CompatibilityDrift {
    CompatibilityDrift::CatalogField {
        model: model.slug.clone(),
        field,
        served: served.to_owned(),
        pinned,
    }
}

pub(in crate::native_auth) fn raw_contract_drift(
    raw: &str,
    model: &str,
) -> Option<CompatibilityDrift> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let fields = [
        (
            "visibility",
            value
                .get("visibility")
                .is_some_and(serde_json::Value::is_string),
        ),
        (
            "shell_type",
            value
                .get("shell_type")
                .is_some_and(serde_json::Value::is_string),
        ),
        (
            "supported_reasoning_levels",
            value
                .get("supported_reasoning_levels")
                .is_some_and(serde_json::Value::is_array),
        ),
        (
            "tool_mode",
            value
                .get("tool_mode")
                .is_none_or(serde_json::Value::is_string),
        ),
        (
            "use_responses_lite",
            value
                .get("use_responses_lite")
                .is_none_or(serde_json::Value::is_boolean),
        ),
        (
            "minimal_client_version",
            value
                .get("minimal_client_version")
                .is_none_or(|v| v.is_null() || v.is_string()),
        ),
        (
            "input_modalities",
            value
                .get("input_modalities")
                .is_none_or(serde_json::Value::is_array),
        ),
    ];
    fields.into_iter().find_map(|(field, valid)| {
        (!valid).then(|| CompatibilityDrift::CatalogField {
            model: model.to_owned(),
            field,
            served: "invalid or missing".to_owned(),
            pinned: "compiled catalog field type",
        })
    })
}
