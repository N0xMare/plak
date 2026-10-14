//! Account, model, and reasoning-effort compatibility helpers.

use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeProvider;

use crate::binding::PlatformProvider;
use crate::platform::platform_model;
use crate::{CliFailure, CliResult};

pub const fn native_compact_facet(provider: NativeProvider) -> bool {
    matches!(provider, NativeProvider::Codex)
}

pub(crate) fn humanize_turn_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("unknown reasoning visibility")
        || lower.contains("assistantphasewithoutreplay")
        || lower.contains("assistant phase without")
        || lower.contains("reasoning without an exact")
        || lower.contains("opaque-only")
        || lower.contains("no compatible replay")
    {
        "This chat's native reasoning cannot be sent to the selected model. Resume the previous session or pick that model again to return to it.".to_owned()
    } else {
        error.to_owned()
    }
}

// The account -> OAuth provider decision lives with the rest of the binding
// vocabulary (`crate::binding`), next to `TargetSelection::route`. It is
// re-exported here so every existing `runtime::oauth_provider_for_account`
// reader keeps its spelling.
pub use crate::binding::oauth_provider_for_account;

/// Parse an effort string from product config or a persisted session record.
///
/// The spellings are the shared ladder (`crate::effort`), including explicit
/// `none`. Returns `(effort, explicit)`. When unset or unknown, effort is
/// Medium for wire fallback only — callers must treat `explicit == false` as
/// “operator has not chosen”. Unknown text is not `none`.
pub fn effort_from_config(value: Option<&str>) -> (ReasoningEffort, bool) {
    let Some(value) = value else {
        return (ReasoningEffort::Medium, false);
    };
    match crate::effort::parse(value) {
        Some("none") => (ReasoningEffort::None, true),
        Some("low") => (ReasoningEffort::Low, true),
        Some("medium") => (ReasoningEffort::Medium, true),
        Some("high") => (ReasoningEffort::High, true),
        Some("xhigh") => (ReasoningEffort::XHigh, true),
        Some("max") => (ReasoningEffort::Max, true),
        Some("ultra") => (ReasoningEffort::Ultra, true),
        _ => (ReasoningEffort::Medium, false),
    }
}

/// The effort a persisted session record stored, or `None` when the record
/// left it unset.
///
/// Every route uses this, not only OpenRouter. An unknown spelling is refused:
/// reconstructing it as a default would send a different effort than the one
/// that was saved.
pub fn effort_from_record(value: Option<&str>) -> CliResult<Option<ReasoningEffort>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let (effort, explicit) = effort_from_config(Some(value));
    if !explicit {
        return Err(CliFailure::new(
            "persisted effort is unknown; exact reconstruction refused",
        ));
    }
    Ok(Some(effort))
}

/// Returns the exact Ready-catalog effort requirement for one OAuth row.
pub fn oauth_model_needs_effort(account: &str, model_id: &str) -> bool {
    if oauth_provider_for_account(account).is_none() {
        return false;
    }
    let m = model_id.to_ascii_lowercase();
    if m.contains("non-reasoning") || m == "grok-build-0.1" {
        return false;
    }
    true
}

pub fn ready_catalog_model_needs_effort(
    ready_catalog: &[plakous_host_protocol::views::ProviderSection],
    account: &str,
    model_id: &str,
) -> Option<bool> {
    ready_catalog
        .iter()
        .find(|section| section.ready && section.account == account)
        .and_then(|section| section.entries.iter().find(|entry| entry.id == model_id))
        .map(|entry| !entry.effort_options.is_empty())
}

/// Heuristic: refuse attach when account and model id clearly belong to
/// different OAuth catalogs (prevents `grok` + `gpt-5.6-luna` death spiral).
pub fn account_model_compatible(account: &str, model_id: &str) -> bool {
    let m = model_id.to_ascii_lowercase();
    let looks_codex = m.starts_with("gpt-")
        || m.contains("codex")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4");
    let looks_grok = m.starts_with("grok");
    let looks_claude = m.starts_with("claude");
    match account {
        "openai" => platform_model(PlatformProvider::OpenAi, model_id).is_some(),
        "anthropic" => platform_model(PlatformProvider::Anthropic, model_id).is_some(),
        "xai-api" => platform_model(PlatformProvider::Xai, model_id).is_some(),
        "codex" | "chatgpt" => !looks_grok && !looks_claude,
        "grok" | "supergrok" | "xai-oauth" => !looks_codex && !looks_claude,
        "claude" => !looks_codex && !looks_grok,
        _ => true,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    use plakous::subscriptions::NativeProvider;

    #[test]
    fn account_model_compatible_rejects_cross_provider() {
        assert!(account_model_compatible("codex", "gpt-5.6-luna"));
        assert!(account_model_compatible("grok", "grok-4.5"));
        assert!(account_model_compatible("claude", "claude-sonnet-5"));
        assert!(!account_model_compatible("grok", "gpt-5.6-luna"));
        assert!(!account_model_compatible("codex", "grok-4.5"));
        assert!(!account_model_compatible("claude", "grok-4.5"));
        assert!(!account_model_compatible("codex", "claude-sonnet-5"));
    }

    #[test]
    fn oauth_account_mapping() {
        assert_eq!(
            oauth_provider_for_account("grok"),
            Some(NativeProvider::Xai)
        );
        assert_eq!(
            oauth_provider_for_account("codex"),
            Some(NativeProvider::Codex)
        );
        assert_eq!(oauth_provider_for_account("openai"), None);
        assert!(
            humanize_turn_error(
                "model preparation failed: unknown reasoning visibility cannot be replayed"
            )
            .contains("Resume the previous session")
        );
        assert_eq!(humanize_turn_error("tool failed"), "tool failed");
    }

    #[test]
    fn effort_parser_treats_none_as_explicit_and_refuses_unknown() {
        assert_eq!(
            effort_from_config(Some("none")),
            (ReasoningEffort::None, true)
        );
        assert_eq!(
            effort_from_config(Some("NONE")),
            (ReasoningEffort::None, true)
        );
        assert_eq!(effort_from_config(None), (ReasoningEffort::Medium, false));
        assert_eq!(
            effort_from_config(Some("turbo")),
            (ReasoningEffort::Medium, false)
        );
        assert_eq!(
            effort_from_config(Some("  high ")),
            (ReasoningEffort::High, true)
        );
        for level in crate::effort::LEVELS {
            let (effort, explicit) = effort_from_config(Some(level));
            assert!(explicit, "{level}");
            assert_eq!(super::super::effort_label(effort), level);
        }
        assert_eq!(effort_from_record(None).unwrap(), None);
        assert_eq!(
            effort_from_record(Some("none")).unwrap(),
            Some(ReasoningEffort::None)
        );
        let refused = effort_from_record(Some("turbo")).unwrap_err();
        assert_eq!(
            refused.message(),
            "persisted effort is unknown; exact reconstruction refused"
        );
        assert!(!refused.message().contains("OpenRouter"));
    }
}
