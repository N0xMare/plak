//! Raising of canonical tool results into each projection's result shape.

use plakous_model::ToolResultPart;
use serde::{Deserialize, Serialize};

use self::claude::{claude_background_notice, claude_cat_n_from_canonical_read};
use self::pi::{pi_background_notice, pi_text_from_canonical_read};
use super::projection::{
    ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionRaisingV2,
};
use crate::{
    FrozenToolRegistry, PortableToolSchemaV2, RegisteredToolIdentity, ToolOutcome,
    ToolRuntimeError, ToolRuntimeErrorKind,
};

mod claude;
mod pi;

/// Raises one canonical process result into the projection's shared shape.
#[allow(
    clippy::too_many_lines,
    reason = "each vendor's background and status lines stay beside the shared shape"
)]
pub(super) fn raise_process_result(
    entry: &ToolProjectionExposureV2,
    outcome: &ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let [ToolResultPart::Text(text)] = outcome.parts() else {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "canonical projection result is not one structured text value",
        ));
    };
    let canonical: CanonicalProcessResultWireV2 =
        serde_json::from_str(text.text()).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "canonical process result is malformed",
                &error,
            )
        })?;
    canonical.validate()?;
    let status = raised_process_status(&canonical.status);
    let mut output = canonical.output;
    if canonical.status == "leftover" {
        let stop = match entry.lowering {
            ToolProjectionLoweringV2::CodexExecCommand
            | ToolProjectionLoweringV2::CodexWriteStdin => {
                "Stop with write_stdin using chars exactly \\u0003."
            }
            ToolProjectionLoweringV2::ClaudeBash => "Stop with TaskStop using task_id.",
            ToolProjectionLoweringV2::PiBash => "Stop with stop using the id.",
            _ => "Stop with kill_command_or_subagent using task_id.",
        };
        output.push('\n');
        output.push_str(stop);
    }
    if entry.lowering == ToolProjectionLoweringV2::PiBash
        && let Some(id) = canonical.session_id
        && let Some(hint) = canonical.foreground_hint_ms
    {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&pi_background_notice(
            canonical.backgrounded_by_user.unwrap_or(false),
            canonical.preempted.unwrap_or(false),
            id,
            hint,
        ));
    }
    if entry.lowering == ToolProjectionLoweringV2::ClaudeBash
        && let Some(id) = canonical.session_id
        && let Some(hint) = canonical.foreground_hint_ms
    {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&claude_background_notice(
            canonical.backgrounded_by_user.unwrap_or(false),
            canonical.preempted.unwrap_or(false),
            id,
            hint,
        ));
    }
    if matches!(
        entry.lowering,
        ToolProjectionLoweringV2::GrokRunTerminalCommand
            | ToolProjectionLoweringV2::ClaudeBash
            | ToolProjectionLoweringV2::ClaudeTaskStop
            | ToolProjectionLoweringV2::PiBash
    ) && let Some(trailer) = process_status_trailer(&canonical.status, canonical.exit_code)
    {
        // Vendor models read the text, not the JSON fields: a failed
        // or killed command says so where they look.
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&trailer);
    }
    let raised = serde_json::to_string(&RaisedProcessResultWireV2 {
        exit_code: canonical.exit_code,
        session_id: if entry.lowering == ToolProjectionLoweringV2::GrokRunTerminalCommand {
            None
        } else {
            canonical.session_id
        },
        task_id: if entry.lowering == ToolProjectionLoweringV2::GrokRunTerminalCommand {
            canonical.session_id.map(|id| format!("p{id}"))
        } else {
            None
        },
        status,
        output,
    })
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Execution,
            "raised process result could not be encoded",
            &error,
        )
    })?;
    entry
        .output_schema
        .parse_instance(&raised)
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "canonical projection result violates its raised V2 contract",
                &error,
            )
        })?;
    let update = outcome.process_session_update();
    let raised = ToolOutcome::success_text(raised)?;
    Ok(match update {
        Some(update) => raised.with_process_session_update(update),
        None => raised,
    })
}

pub(super) fn validate_model_content_identity(
    registry: &FrozenToolRegistry,
    identity: &RegisteredToolIdentity,
    outcome: ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    if outcome.kind() != crate::ToolOutcomeKind::Success {
        return Ok(outcome);
    }
    let definition = registry.definition(identity.name()).ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "model-content projection source definition is absent",
        )
    })?;
    outcome.validate_contract(definition.output())
}

/// Raises one canonical read result into the presentation's text: Claude's
/// `cat -n` lines or pi's plain text with a continuation line.
pub(super) fn raise_read_observation(
    entry: &ToolProjectionExposureV2,
    outcome: &ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let [ToolResultPart::Text(text)] = outcome.parts() else {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "read observation is not one text value",
        ));
    };
    let numbered = if entry.lowering == ToolProjectionLoweringV2::PiRead {
        pi_text_from_canonical_read(text.text())?
    } else {
        claude_cat_n_from_canonical_read(text.text())?
    };
    let update = outcome.process_session_update();
    if entry.raising() == ToolProjectionRaisingV2::TextResultObjectV1 {
        return raise_text_result_object_from_result(&numbered, update);
    }
    // TextBlock forbids empty strings for Identity raising. Direct wraps an empty
    // result above; nested RawUtf8 bypasses this projection and retains neutral JSON.
    let body = if numbered.is_empty() {
        "\n".to_owned()
    } else {
        numbered
    };
    let raised = ToolOutcome::success_text(body)?;
    let raised = match update {
        Some(update) => raised.with_process_session_update(update),
        None => raised,
    };
    if entry.raising() == ToolProjectionRaisingV2::Identity {
        return raise_identity(entry, raised);
    }
    if entry.raising() == ToolProjectionRaisingV2::RawUtf8V1 {
        return Ok(raised);
    }
    Err(ToolRuntimeError::new(
        ToolRuntimeErrorKind::Execution,
        "read observation raising is not enrolled",
    ))
}

pub(super) fn raise_identity(
    entry: &ToolProjectionExposureV2,
    outcome: ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    if outcome.kind() == crate::ToolOutcomeKind::Success {
        let [ToolResultPart::Text(text)] = outcome.parts() else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "identity projection result is not one structured text value",
            ));
        };
        entry
            .output_schema
            .parse_instance(text.text())
            .map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Execution,
                    "identity projection result violates its raised V2 contract",
                    &error,
                )
            })?;
    }
    Ok(outcome)
}

pub(super) fn raise_text_result_object(
    outcome: ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    if outcome.kind() != crate::ToolOutcomeKind::Success {
        return Ok(outcome);
    }
    let [ToolResultPart::Text(text)] = outcome.parts() else {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "canonical projection result is not one text value",
        ));
    };
    let update = outcome.process_session_update();
    raise_text_result_object_from_result(text.text(), update)
}

/// Whether every `{"result": <string>}` object is an instance of `schema`.
///
/// Construction checks this once, so raising needs no per-result validation:
/// the raised object is bounded by the canonical output contract it wraps, and
/// the runtime's per-result observation cut (not the semantic-JSON string
/// bound) brings it under the model-visible bound.
pub(super) fn admits_every_text_result(schema: &PortableToolSchemaV2) -> bool {
    let document = schema.document().value();
    document.get("properties") == Some(&serde_json::json!({"result": {"type": "string"}}))
        && document.get("required").is_none_or(|required| {
            required
                .as_array()
                .is_some_and(|required| required.iter().all(|name| name == "result"))
        })
}

fn raise_text_result_object_from_result(
    result: &str,
    update: Option<crate::internal::ProcessSessionUpdate>,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let raised =
        serde_json::to_string(&serde_json::json!({"result": result})).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "raised text result could not be encoded",
                &error,
            )
        })?;
    let raised = ToolOutcome::raised_text(raised)?;
    Ok(match update {
        Some(update) => raised.with_process_session_update(update),
        None => raised,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalProcessResultWireV2 {
    status: String,
    exit_code: Option<i32>,
    session_id: Option<u64>,
    cursor: u64,
    output: String,
    foreground_hint_ms: Option<u64>,
    preempted: Option<bool>,
    backgrounded_by_user: Option<bool>,
}

impl CanonicalProcessResultWireV2 {
    fn validate(&self) -> Result<(), ToolRuntimeError> {
        let valid = self.cursor < u64::MAX
            && match self.status.as_str() {
                "running" => self.session_id.is_some() && self.exit_code.is_none(),
                "leftover" => self.session_id.is_some(),
                "exited" => self.session_id.is_none(),
                "terminated" | "cancelled" | "timed_out" | "output_limit" | "failed"
                | "outcome_unknown" | "cleanup_unresolved" => {
                    self.session_id.is_none() && self.exit_code.is_none()
                }
                _ => false,
            };
        if !valid {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "canonical process result status is inconsistent",
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct RaisedProcessResultWireV2 {
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// Set only when the process did not run to its own exit.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    output: String,
}

/// The raised status for a process that ended other than by its own exit.
fn raised_process_status(status: &str) -> Option<&'static str> {
    match status {
        "leftover" => Some("leftover"),
        "timed_out" => Some("timed_out"),
        "terminated" => Some("terminated"),
        "cancelled" => Some("cancelled"),
        "output_limit" => Some("output_limit"),
        "failed" => Some("failed"),
        "outcome_unknown" => Some("outcome_unknown"),
        "cleanup_unresolved" => Some("cleanup_unresolved"),
        _ => None,
    }
}

/// The one-line text trailer for a nonzero exit or an abnormal end.
fn process_status_trailer(status: &str, exit_code: Option<i32>) -> Option<String> {
    match status {
        "exited" => exit_code
            .filter(|code| *code != 0)
            .map(|code| format!("[exit {code}]")),
        "timed_out" => Some(String::from("[timed out; the process group was killed]")),
        "terminated" => Some(String::from("[killed]")),
        "cancelled" => Some(String::from("[cancelled]")),
        "output_limit" => Some(String::from("[killed: output limit]")),
        "failed" => Some(String::from(
            "[failed: the process could not be supervised]",
        )),
        "outcome_unknown" => Some(String::from(
            "[outcome unknown: the previous process owner is unavailable]",
        )),
        "cleanup_unresolved" => Some(String::from("[cleanup unresolved]")),
        _ => None,
    }
}
