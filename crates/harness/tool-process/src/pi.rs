//! pi 1.1.0 process tool schemas: `bash` as pi ships it, plus the `wait` and
//! `stop` tools pi does not have, in the same plain shape. Property
//! descriptions are pi's where pi has the field, and state our bounds.
//! pi is MIT; see the README License section.

use plakous_tools::object_schema;
use serde_json::{Value, json};

pub(super) fn bash_schema() -> Value {
    object_schema(
        json!({
            "command": {
                "description": "Shell command to execute",
                "type": "string"
            },
            "timeout": {
                "description": "Seconds to wait for the result before returning (not a kill timer); 0 starts the command in the background",
                "type": "integer",
                "minimum": 0
            }
        }),
        &["command"],
    )
}

pub(super) fn wait_schema() -> Value {
    object_schema(
        json!({
            "ids": {
                "description": "Process ids returned by bash; omit for every unreported one",
                "type": "array",
                "minItems": 1,
                "maxItems": 128,
                "items": {"type": "string"}
            },
            "timeout_ms": {
                "description": "Omit for 300000; clamped to 3600000; 0 returns a snapshot",
                "type": "integer",
                "minimum": 0
            }
        }),
        &[],
    )
}

pub(super) fn stop_schema() -> Value {
    object_schema(
        json!({
            "id": {
                "description": "Process id returned by bash",
                "type": "string"
            }
        }),
        &["id"],
    )
}
