//! Tool schemas adapted from Grok Build 1.0.46 (Apache-2.0,
//! <https://github.com/xai-org/grok-build>); see the README License section.
//! Portable schemas drop draft-07, format and default annotations, close objects,
//! nullable hints use omission rather than null, and descriptions state our bound
//! budgets and process-only surface.

use plakous_tools::object_schema;
use serde_json::{Value, json};

pub(super) fn start_schema() -> Value {
    object_schema(
        json!({
            "command": {
                "description": "The bash command to run.",
                "type": "string"
            },
            "block_until_ms": {
                "description": "How long to observe before moving the command to the background, in milliseconds. Default is the construction-bound foreground budget stated in the tool description; clamped to 600000. 0 selects a 250 ms first observation. Includes shell startup.",
                "type": "integer",
                "minimum": 0
            },
            "description": {
                "description": "One short sentence that says what this command does and why you run it.",
                "type": "string"
            }
        }),
        &["command", "description"],
    )
}

pub(super) fn stop_schema() -> Value {
    object_schema(
        json!({
            "task_id": {
                "description": "Process task ID (N or pN) to stop. Agents and monitors are not accepted.",
                "type": "string"
            }
        }),
        &["task_id"],
    )
}

pub(super) fn output_schema() -> Value {
    object_schema(
        json!({
            "task_ids": {
                "description": "One or more process task IDs (N or pN). Agents and monitors are not accepted.",
                "type": "array",
                "minItems": 1,
                "maxItems": 128,
                "items": {
                    "type": "string"
                }
            },
            "timeout_ms": {
                "description": "How long to await all requested processes finishing, in milliseconds; clamped to 3600000 (1 hour). Omit or pass 0 for an immediate snapshot.",
                "type": "integer",
                "minimum": 0
            }
        }),
        &["task_ids"],
    )
}
