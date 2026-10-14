//! v1 JSONL envelope: the frame every `plak agent` line carries and
//! the outbound lines whose shape is fixed by the handshake rather than by a
//! host observation.
//!
//! Framing is one JSON object per line in both directions. Every line carries
//! `v`, and every outbound line carries `kind`. Key order within the object is
//! not part of the protocol. The presentation major is independent of the
//! product `SQLite` schema and of Plakous.

use serde_json::{Map, Value};

/// Presentation schema major. Independent of the product `SQLite` schema and of Plakous.
pub const SCHEMA_V: u32 = 1;

/// Same bound as `plak run` prompt admission.
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// Outbound kinds a sink must never drop: the handshake, the tool and error
/// records a client reconciles against, turn accounting and lifecycle
/// evidence, the tree, and the retry that tells a client to discard streamed
/// assistant text.
pub const REQUIRED_STDOUT_KINDS: [&str; 11] = [
    "hello",
    "ready",
    "tool",
    "error",
    "turn",
    "turn_lifecycle",
    "tree",
    "bye",
    "model_refusal",
    "recovered",
    "retry",
];

/// One stdout JSONL object. Machine-only; never terminal control.
#[must_use]
pub fn encode_object(mut fields: Map<String, Value>) -> String {
    let mut object = Map::new();
    object.insert("v".to_owned(), Value::from(SCHEMA_V));
    if let Some(kind) = fields.remove("kind") {
        object.insert("kind".to_owned(), kind);
    }
    if let Some(id) = fields.remove("id") {
        object.insert("id".to_owned(), id);
    }
    object.extend(fields);
    Value::Object(object).to_string()
}

/// The `error` line: a typed refusal that leaves the connection up.
#[must_use]
pub fn diagnostic_line(id: Option<&str>, message: &str) -> String {
    let mut fields = Map::new();
    fields.insert("kind".to_owned(), Value::from("error"));
    if let Some(id) = id.filter(|id| !id.is_empty()) {
        fields.insert("id".to_owned(), Value::from(id));
    }
    fields.insert("message".to_owned(), Value::from(message));
    encode_object(fields)
}

/// The `hello` line: the first outbound object of a session.
#[must_use]
#[expect(
    clippy::too_many_arguments,
    reason = "one wire object; its fields are the protocol"
)]
pub fn hello_line(
    id: Option<&str>,
    session_id: &str,
    tree_id: &str,
    account: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    composition: &str,
    tree_state: &str,
    javascript_host: Option<&str>,
    javascript_host_target: Option<&str>,
    python_host: Option<&str>,
    python_host_target: Option<&str>,
    elapsed_ms: Option<u64>,
) -> String {
    let mut fields = Map::new();
    fields.insert("kind".to_owned(), Value::from("hello"));
    if let Some(id) = id.filter(|id| !id.is_empty()) {
        fields.insert("id".to_owned(), Value::from(id));
    }
    fields.insert("session_id".to_owned(), Value::from(session_id));
    fields.insert("tree_id".to_owned(), Value::from(tree_id));
    if let Some(account) = account {
        fields.insert("account".to_owned(), Value::from(account));
    }
    if let Some(model) = model {
        fields.insert("model".to_owned(), Value::from(model));
    }
    if let Some(effort) = effort {
        fields.insert("effort".to_owned(), Value::from(effort));
    }
    fields.insert("composition".to_owned(), Value::from(composition));
    fields.insert("tree_state".to_owned(), Value::from(tree_state));
    if let Some(javascript_host) = javascript_host {
        fields.insert("javascript_host".to_owned(), Value::from(javascript_host));
    }
    if let Some(javascript_host_target) = javascript_host_target {
        fields.insert(
            "javascript_host_target".to_owned(),
            Value::from(javascript_host_target),
        );
    }
    if let Some(python_host) = python_host {
        fields.insert("python_host".to_owned(), Value::from(python_host));
    }
    if let Some(python_host_target) = python_host_target {
        fields.insert(
            "python_host_target".to_owned(),
            Value::from(python_host_target),
        );
    }
    if let Some(elapsed_ms) = elapsed_ms {
        fields.insert("elapsed_ms".to_owned(), Value::from(elapsed_ms));
    }
    encode_object(fields)
}

/// The `ready` line: the host will accept commands.
#[must_use]
pub fn ready_line(id: Option<&str>, elapsed_ms: Option<u64>) -> String {
    let mut fields = Map::new();
    fields.insert("kind".to_owned(), Value::from("ready"));
    if let Some(id) = id.filter(|id| !id.is_empty()) {
        fields.insert("id".to_owned(), Value::from(id));
    }
    if let Some(elapsed_ms) = elapsed_ms {
        fields.insert("elapsed_ms".to_owned(), Value::from(elapsed_ms));
    }
    encode_object(fields)
}

/// The `bye` line: the last outbound object of a session.
#[must_use]
pub fn bye_line(id: Option<&str>) -> String {
    let mut fields = Map::new();
    fields.insert("kind".to_owned(), Value::from("bye"));
    if let Some(id) = id.filter(|id| !id.is_empty()) {
        fields.insert("id".to_owned(), Value::from(id));
    }
    encode_object(fields)
}

/// Per-event JSONL correlation slots.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JsonlCorrelation<'a> {
    /// The in-flight prompt's id, when a prompt is open.
    pub prompt_id: Option<&'a str>,
    /// Omit turn correlation when no proven prompt origin is available.
    pub omit_unknown_turn_id: bool,
    /// The command currently being answered.
    pub command_id: Option<&'a str>,
    /// Set when stdin admits `tree` / `resume_tree`; cleared after the matching
    /// snapshot is projected. Unsolicited topology revisions omit `id`.
    pub pending_tree_snapshot_id: Option<&'a str>,
}

impl<'a> JsonlCorrelation<'a> {
    /// Id for a turn-scoped event: the prompt's when one is in flight, else
    /// the current command's.
    #[must_use]
    pub fn turn_scoped_id(&self) -> Option<&'a str> {
        self.prompt_id
            .or_else(|| {
                (!self.omit_unknown_turn_id)
                    .then_some(self.command_id)
                    .flatten()
            })
            .filter(|id| !id.is_empty())
    }

    /// Id for a command answer: only the command's.
    #[must_use]
    pub fn command_scoped_id(&self) -> Option<&'a str> {
        self.command_id.filter(|id| !id.is_empty())
    }

    /// Id for a tree snapshot: present only when the snapshot was asked for.
    #[must_use]
    pub fn tree_snapshot_id(&self, solicited: bool) -> Option<&'a str> {
        if !solicited {
            return None;
        }
        self.pending_tree_snapshot_id.filter(|id| !id.is_empty())
    }

    /// Write a non-empty `id` into an outbound object under construction.
    pub fn insert_id(fields: &mut Map<String, Value>, id: Option<&str>) {
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            fields.insert("id".to_owned(), Value::from(id));
        }
    }
}

/// True when this host event must not be dropped on the JSONL sink.
#[must_use]
pub fn required_stdout_kind(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|value| value.get("kind").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|kind| REQUIRED_STDOUT_KINDS.contains(&kind.as_str()))
}
