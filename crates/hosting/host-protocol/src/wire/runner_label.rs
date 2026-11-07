//! Runner identity at the authenticated client boundary.

use serde::{Deserialize, Serialize};

/// A configured label and its pinned identity; only identity is authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerLabel {
    /// Canonical configured alias, or saved id when no alias remains.
    pub name: String,
    /// Pinned identity; `local` is the host's local runner, never the client.
    pub runner_id: String,
}

impl RunnerLabel {
    /// The host-local runner.
    #[must_use]
    pub fn local() -> Self {
        Self {
            name: "local".to_owned(),
            runner_id: "local".to_owned(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn runner_label_has_exact_closed_local_and_pinned_shapes() {
        assert_eq!(
            serde_json::to_string(&RunnerLabel::local()).unwrap(),
            r#"{"name":"local","runner_id":"local"}"#
        );
        let remote = r#"{"name":"machine","runner_id":"pinned-id"}"#;
        assert_eq!(
            serde_json::to_string(&serde_json::from_str::<RunnerLabel>(remote).unwrap()).unwrap(),
            remote
        );
        for invalid in [
            r#"{"name":"machine"}"#,
            r#"{"runner_id":"pinned-id"}"#,
            r#"{"name":"machine","runner_id":"pinned-id","address":"hidden"}"#,
        ] {
            assert!(serde_json::from_str::<RunnerLabel>(invalid).is_err());
        }
        let schema = serde_json::to_value(schemars::schema_for!(RunnerLabel)).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["name", "runner_id"]));
        assert_eq!(schema["additionalProperties"], false);
    }
}
