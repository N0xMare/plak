//! The cell protocol between Plak and a worker process.
//!
//! The worker opens with [`WorkerFrame::Hello`]; the host answers once with
//! [`HostFrame::Configure`] before any cell starts. Every later frame names
//! the cell it belongs to. A worker that ends its stream without a
//! [`WorkerFrame::CellDone`] for a running cell has lost it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::process_resources::ProcessResourceUsage as ResourceUsage;

/// The protocol revision both sides name in `Hello` and `Configure`.
pub const CELL_PROTOCOL_REVISION: &str = "plakous-cell-protocol.v2";

/// A worker build: the engine and adapter its `Hello` names, and the engine
/// version its engine contract records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerIdentity {
    /// Executable name used to locate the worker artifact.
    pub binary: &'static str,
    /// Interpreter family named in the handshake.
    pub engine: &'static str,
    /// Interpreter version recorded in the engine contract.
    pub version: &'static str,
    /// Exact adapter revision named in the handshake.
    pub adapter: &'static str,
}

/// The `QuickJS` worker, `plakous-js-host`.
pub const QUICKJS_WORKER: WorkerIdentity = WorkerIdentity {
    binary: "plakous-js-host",
    engine: "quickjs",
    version: "rquickjs-0.12",
    adapter: "plakous-js-host-bootstrap.v7",
};

/// The micro-monty worker, `plakous-py-host`.
pub const MONTY_WORKER: WorkerIdentity = WorkerIdentity {
    binary: "plakous-py-host",
    engine: "micro-monty",
    // The worker verifies this version and build ID against the linked engine.
    version: "0.0.2+06717fdeb70bb48b5e5461b21b1816428052fc4f71846c8b79cefbb14713231d",
    adapter: "plakous.micro-monty-python-adapter.v1",
};

/// Parent to worker.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum HostFrame {
    /// Once, after `Hello`: the protocol revision and the engine's limits.
    Configure {
        /// Exact cell protocol revision.
        revision: String,
        /// Engine identity and resource limits.
        engine: EngineConfig,
    },
    /// Clears Python globals before an SDK cell, retaining the warm worker.
    Reset,
    /// Starts one cell. `window` is the engine-parked yield interval in
    /// milliseconds, when the presentation yields inside the engine.
    StartCell {
        /// Host-assigned cell identity.
        cell: u64,
        /// Source admitted for this execution.
        source: String,
        /// Visible bindings and retained state.
        prelude: CellPrelude,
        /// Engine-parked observation interval in milliseconds.
        window: Option<u64>,
    },
    /// The outcome of one nested call: its value, or the failure the cell
    /// observes as a thrown error.
    NestedResult {
        /// Owning cell identity.
        cell: u64,
        /// Correlated nested call identity.
        call: u64,
        /// Returned value or model-visible failure.
        outcome: Result<CellValue, String>,
    },
    /// Resumes an engine-parked yield for another window.
    Resume {
        /// Parked cell identity.
        cell: u64,
        /// Next observation interval in milliseconds.
        window: u64,
    },
    /// Cooperative interrupt of the running cell.
    Cancel {
        /// Cell to interrupt.
        cell: u64,
    },
    /// Ends the worker; it answers with `ResourceReport` and exits.
    Shutdown,
}

/// Worker to parent.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum WorkerFrame {
    /// First frame: the protocol revision, engine and adapter identities.
    Hello {
        /// Exact cell protocol revision.
        revision: String,
        /// Interpreter family.
        engine: String,
        /// Exact adapter revision.
        adapter: String,
    },
    /// A nested call the cell made; the host validates `name`.
    NestedCall {
        /// Owning cell identity.
        cell: u64,
        /// Worker-assigned nested call identity.
        call: u64,
        /// Exposed tool name.
        name: String,
        /// Tool input before host validation.
        input: CellValue,
    },
    /// Output in production order.
    Output {
        /// Producing cell identity.
        cell: u64,
        /// One ordered output item.
        item: OutputItem,
    },
    /// What became of one `NestedResult`, sent once the worker knows.
    Delivery {
        /// Owning cell identity.
        cell: u64,
        /// Correlated nested call identity.
        call: u64,
        /// What the interpreter proved about acceptance.
        verdict: DeliveryVerdict,
    },
    /// The cell suspended on these nested calls and waits for their results.
    Waiting {
        /// Suspended cell identity.
        cell: u64,
        /// Outstanding nested calls blocking execution.
        calls: Vec<u64>,
    },
    /// The cell parked at a yield and waits for `Resume`.
    Yielded {
        /// Parked cell identity.
        cell: u64,
    },
    /// The cell's one terminal.
    CellDone {
        /// Completed cell identity.
        cell: u64,
        /// The cell's definite terminal outcome.
        outcome: CellOutcome,
    },
    /// The worker lost its evaluator; nothing after this is reliable.
    HostLost {
        /// Closed loss category.
        class: LossClass,
        /// Diagnostic describing the loss.
        message: String,
    },
    /// Self-reported usage, sent once in answer to `Shutdown`.
    ResourceReport {
        /// Worker process resource sample.
        usage: ResourceUsage,
    },
}

/// Engine limits the host declares and the worker checks against its own.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "engine", rename_all = "snake_case")]
pub enum EngineConfig {
    /// The `QuickJS` worker.
    Quickjs {
        /// Maximum interpreter heap bytes.
        heap_bytes: u64,
        /// Maximum interpreter stack bytes.
        stack_bytes: u64,
    },
    /// The Monty worker: the interpreter's soft memory limit.
    Monty {
        /// Soft interpreter memory bound in bytes.
        memory_bytes: u64,
    },
}

impl EngineConfig {
    /// The `QuickJS` worker's exact limits.
    pub const QUICKJS: Self = Self::Quickjs {
        heap_bytes: 64 * 1024 * 1024,
        stack_bytes: 512 * 1024,
    };

    /// The Monty worker's exact limits.
    pub const MONTY: Self = Self::Monty {
        memory_bytes: 256 * 1024 * 1024,
    };
}

/// Per-cell bindings.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "engine", rename_all = "snake_case")]
pub enum CellPrelude {
    /// `QuickJS`: the visible tool declarations and the retained `store` map.
    Quickjs {
        /// Visible tool declarations.
        tools: Vec<Value>,
        /// Retained session values supplied to the cell.
        #[serde(
            serialize_with = "crate::stored::serialize",
            deserialize_with = "crate::stored::deserialize"
        )]
        stored: HashMap<String, Value>,
    },
    /// Monty: the session's tool wrappers, on the session's first cell only.
    Monty {
        /// Wrappers installed only on the session's first cell.
        bootstrap: Option<MontyBootstrap>,
    },
    /// Fresh Python SDK globals; the client sends `Reset` before `StartCell`.
    MontySdk {
        /// Generated SDK wrappers for the fresh globals.
        bootstrap: MontyBootstrap,
    },
}

/// The Python wrappers a Monty session installs before its first cell: the
/// generated source and the private host functions it calls.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MontyBootstrap {
    /// Generated Python wrapper source.
    pub source: String,
    /// Private host functions the source calls.
    pub bindings: Vec<MontyBinding>,
}

/// One private host function: the tool it calls and the argument it takes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MontyBinding {
    /// Private Python function name.
    pub function: String,
    /// Exposed tool name resolved by the host.
    pub tool: String,
    /// Required argument shape.
    pub input: BindingInput,
}

/// The single argument a bound Python tool takes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingInput {
    /// A structured tool receives a dictionary.
    Dictionary,
    /// A freeform tool receives its exact UTF-8 input string.
    String,
}

/// One unit of cell output.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    /// Model-visible text.
    Text {
        /// Exact output text.
        text: String,
    },
    /// Model-visible image reference.
    Image {
        /// Image URL or data URL.
        url: String,
        /// Optional image detail hint.
        detail: Option<String>,
    },
    /// Model-visible audio reference.
    Audio {
        /// Audio URL or data URL.
        url: String,
    },
    /// Operator notification requiring host acknowledgement.
    Notify {
        /// Notification text.
        text: String,
    },
}

/// What a worker proved about one nested result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "verdict", content = "message", rename_all = "snake_case")]
pub enum DeliveryVerdict {
    /// The interpreter took the exact result.
    Accepted,
    /// The result named no pending call of the cell; the cell is undisturbed.
    Refused(String),
    /// The result provably never reached the interpreter.
    NotDelivered(String),
    /// The cell ended, or abandoned the batch carrying the result, before the
    /// interpreter observed it.
    NotObserved(String),
}

/// A cell's terminal.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CellOutcome {
    /// A host or worker resource bound, never inferred from Python exception text.
    Stopped {
        /// Resource bound that ended execution.
        reason: CellStopReason,
    },
    /// Successful execution and its retained state.
    Completed {
        /// Optional displayed return value.
        value: Option<String>,
        /// Session values retained after success.
        #[serde(
            serialize_with = "crate::stored::serialize",
            deserialize_with = "crate::stored::deserialize"
        )]
        stored: HashMap<String, Value>,
    },
    /// Definite execution failure and remaining session state.
    Failed {
        /// Model-visible error text.
        message: String,
        /// Session values retained after failure.
        #[serde(
            serialize_with = "crate::stored::serialize",
            deserialize_with = "crate::stored::deserialize"
        )]
        stored: HashMap<String, Value>,
        /// Whether the known terminal also cost the evaluator generation.
        generation_lost: bool,
    },
}

/// Typed resource stop for a cell with a known terminal outcome.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CellStopReason {
    /// The total foreground execution budget expired.
    ForegroundBudget,
}

/// Why a worker lost its evaluator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LossClass {
    /// A worker resource bound was exceeded.
    ResourceLimit,
    /// A frame or correlation violated the cell protocol.
    ProtocolViolation,
    /// The interpreter or its process failed.
    EngineFailure,
}

impl LossClass {
    /// The class of a loss described by `message`.
    #[must_use]
    pub fn of(message: &str) -> Self {
        if message.starts_with("resource limit") {
            Self::ResourceLimit
        } else {
            Self::EngineFailure
        }
    }
}

/// Deepest container nesting a nested-call input or result may have. A
/// tagged value costs up to three JSON levels per container, so this keeps
/// every frame below `serde_json`'s 128-level recursion limit; a deeper value
/// is refused for its one call, inside the cell, instead of losing the worker.
pub const MAX_CELL_VALUE_DEPTH: usize = 40;

/// The container nesting of `value`: zero for a scalar. Iterative, so an
/// arbitrarily deep value cannot exhaust the stack.
#[must_use]
pub fn json_depth(value: &Value) -> usize {
    let mut deepest = 0;
    let mut pending = vec![(value, 0_usize)];
    while let Some((value, depth)) = pending.pop() {
        let children: Box<dyn Iterator<Item = &Value>> = match value {
            Value::Array(items) => Box::new(items.iter()),
            Value::Object(map) => Box::new(map.values()),
            _ => continue,
        };
        deepest = deepest.max(depth + 1);
        pending.extend(children.map(|child| (child, depth + 1)));
    }
    deepest
}

/// A value crossing the protocol in either direction.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CellValue {
    /// JSON null or Python None.
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    Int(i64),
    /// An integer outside `i64`, in decimal.
    BigInt(String),
    /// A floating-point number.
    Float(f64),
    /// A UTF-8 string.
    Str(String),
    /// Binary data.
    Bytes(Vec<u8>),
    /// An ordered sequence.
    List(Vec<CellValue>),
    /// String-keyed dictionary entries.
    Dict(Vec<(String, CellValue)>),
    /// A named record with ordered fields.
    Record {
        /// Record type name.
        name: String,
        /// Field names and values.
        fields: Vec<(String, CellValue)>,
    },
}

impl From<Value> for CellValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(value),
            Value::Number(number) => {
                if let Some(value) = number.as_i64() {
                    Self::Int(value)
                } else if let Some(value) = number.as_u64() {
                    Self::BigInt(value.to_string())
                } else {
                    Self::Float(number.as_f64().unwrap_or(f64::NAN))
                }
            }
            Value::String(value) => Self::Str(value),
            Value::Array(values) => Self::List(values.into_iter().map(Self::from).collect()),
            Value::Object(map) => Self::Dict(
                map.into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl TryFrom<CellValue> for Value {
    type Error = String;

    /// The JSON form of a value; bytes and records have none.
    fn try_from(value: CellValue) -> Result<Self, String> {
        Ok(match value {
            CellValue::Null => Self::Null,
            CellValue::Bool(value) => Self::Bool(value),
            CellValue::Int(value) => Self::from(value),
            CellValue::BigInt(value) => Self::from(
                value
                    .parse::<u64>()
                    .map_err(|_| "cell integer has no JSON form".to_owned())?,
            ),
            CellValue::Float(value) => serde_json::Number::from_f64(value)
                .map(Self::Number)
                .ok_or_else(|| "cell float has no JSON form".to_owned())?,
            CellValue::Str(value) => Self::String(value),
            CellValue::List(values) => Self::Array(
                values
                    .into_iter()
                    .map(Self::try_from)
                    .collect::<Result<_, _>>()?,
            ),
            CellValue::Dict(entries) => Self::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| Ok((key, Self::try_from(value)?)))
                    .collect::<Result<_, String>>()?,
            ),
            CellValue::Bytes(_) | CellValue::Record { .. } => {
                return Err("cell value has no JSON form".into());
            }
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "protocol round-trip tests"
)]
mod tests {
    use super::*;

    #[test]
    fn json_values_round_trip_through_cell_values_byte_for_byte() {
        let text = r#"{"a":1,"b":-2,"c":1.0,"d":1.5e300,"e":18446744073709551615,"f":[true,null,"s"],"g":{"h":{}}}"#;
        let json: Value = serde_json::from_str(text).unwrap();
        let cell = CellValue::from(json.clone());
        let framed = serde_json::to_vec(&cell).unwrap();
        let back: CellValue = serde_json::from_slice(&framed).unwrap();
        let json_back = Value::try_from(back).unwrap();
        assert_eq!(json_back, json);
        assert_eq!(
            serde_json::to_string(&json_back).unwrap(),
            serde_json::to_string(&json).unwrap()
        );
    }

    #[test]
    fn values_without_a_json_form_are_refused() {
        assert!(Value::try_from(CellValue::Bytes(vec![1])).is_err());
        assert!(
            Value::try_from(CellValue::Record {
                name: "r".into(),
                fields: Vec::new(),
            })
            .is_err()
        );
        assert!(Value::try_from(CellValue::Float(f64::INFINITY)).is_err());
    }

    #[test]
    fn frames_carry_their_tag() {
        let frame = HostFrame::Cancel { cell: 7 };
        let encoded = serde_json::to_value(&frame).unwrap();
        assert_eq!(encoded, serde_json::json!({"frame": "cancel", "cell": 7}));
        let verdict = WorkerFrame::Delivery {
            cell: 1,
            call: 2,
            verdict: DeliveryVerdict::Refused("no such call".into()),
        };
        let decoded: WorkerFrame =
            serde_json::from_value(serde_json::to_value(&verdict).unwrap()).unwrap();
        assert_eq!(decoded, verdict);
    }
}
