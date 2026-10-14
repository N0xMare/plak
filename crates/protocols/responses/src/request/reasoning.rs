//! Responses-family `reasoning` object wire helpers.

use serde::Serialize;
use serde_json::{Map, Value};

/// Documented Responses-family `reasoning.effort` labels.
///
/// Token budgets are **not** mapped here; dialects fail closed on those before
/// calling this type. Dialects also fail closed when an exact model does not
/// admit a higher level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningEffortWire {
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// Extra-high effort (`xhigh`).
    XHigh,
    /// Maximum effort (`max`).
    Max,
    /// Ultra effort (`ultra`).
    Ultra,
    /// Explicitly no reasoning (`none`).
    None,
}

impl ReasoningEffortWire {
    /// Returns the exact wire string for `reasoning.effort`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
            Self::None => "none",
        }
    }
}

/// Default `reasoning.summary` used by OpenAI Platform, xAI, and OpenRouter.
pub const REASONING_SUMMARY_AUTO: &str = "auto";

/// Builds the Responses `reasoning` JSON object (optional effort + summary).
///
/// # Examples
///
/// ```
/// use plakous_responses::{REASONING_SUMMARY_AUTO, ReasoningEffortWire, reasoning_object};
/// use serde_json::json;
///
/// let value = reasoning_object(Some(ReasoningEffortWire::Medium), REASONING_SUMMARY_AUTO);
/// assert_eq!(value, json!({"effort": "medium", "summary": "auto"}));
/// ```
#[must_use]
pub fn reasoning_object(effort: Option<ReasoningEffortWire>, summary: &str) -> Value {
    let mut map = Map::new();
    if let Some(effort) = effort {
        map.insert(
            "effort".to_owned(),
            Value::String(effort.as_str().to_owned()),
        );
    }
    map.insert("summary".to_owned(), Value::String(summary.to_owned()));
    Value::Object(map)
}

/// Serializable `reasoning` config for typed request bodies (xAI / OpenRouter).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ReasoningConfigWire {
    /// Wire effort label when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<&'static str>,
    /// Summary mode (typically [`REASONING_SUMMARY_AUTO`]).
    pub summary: &'static str,
}

impl ReasoningConfigWire {
    /// Builds config with optional effort and the shared `summary: "auto"`.
    #[must_use]
    pub const fn auto_summary(effort: Option<ReasoningEffortWire>) -> Self {
        Self {
            effort: match effort {
                Some(level) => Some(level.as_str()),
                None => None,
            },
            summary: REASONING_SUMMARY_AUTO,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn effort_labels_are_stable() {
        assert_eq!(ReasoningEffortWire::Low.as_str(), "low");
        assert_eq!(ReasoningEffortWire::Medium.as_str(), "medium");
        assert_eq!(ReasoningEffortWire::High.as_str(), "high");
        assert_eq!(ReasoningEffortWire::XHigh.as_str(), "xhigh");
        assert_eq!(ReasoningEffortWire::Max.as_str(), "max");
        assert_eq!(ReasoningEffortWire::Ultra.as_str(), "ultra");
        assert_eq!(ReasoningEffortWire::None.as_str(), "none");
    }

    #[test]
    fn reasoning_object_with_and_without_effort() {
        assert_eq!(
            reasoning_object(Some(ReasoningEffortWire::High), REASONING_SUMMARY_AUTO),
            json!({"effort": "high", "summary": "auto"})
        );
        assert_eq!(
            reasoning_object(None, REASONING_SUMMARY_AUTO),
            json!({"summary": "auto"})
        );
    }

    #[test]
    fn reasoning_config_wire_serializes_like_platform_fixture() {
        let config = ReasoningConfigWire::auto_summary(Some(ReasoningEffortWire::Medium));
        let value = serde_json::to_value(config).expect("serialize");
        assert_eq!(value, json!({"effort": "medium", "summary": "auto"}));
        let bare = ReasoningConfigWire::auto_summary(None);
        assert_eq!(
            serde_json::to_value(bare).expect("serialize"),
            json!({"summary": "auto"})
        );
    }
}
