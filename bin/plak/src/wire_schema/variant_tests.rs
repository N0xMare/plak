//! Every wire v2 frame kind, and every observation, intent, op, answer and
//! refusal variant, Plak's extension traffic included, crosses the wire as
//! one line and decodes back to itself. A reader decodes frames through
//! serde's buffered tagged content, which some shapes cannot pass (a
//! 128-bit integer did not); a value of each variant is built from this
//! build's own schema, every optional field filled, so a new variant or
//! field is covered as soon as it exists.

use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{TranscriptPatch, TranscriptScope};
use plakous_host_protocol::wire::{
    ClientFrame, HostFrame, Op, Outcome, decode_host_line, decode_line, encode_line,
};
use plakous_host_protocol::{SessionIntent, SessionObservation};
use serde_json::{Map, Value, json};

use super::*;

type Observation = SessionObservation<PlakExtension>;
type Intent = SessionIntent<PlakExtension>;
type Host = HostFrame<PlakExtension>;
type Client = ClientFrame<PlakExtension>;

/// How deep optional fields are filled; below it only what is required.
const FILLED_DEPTH: usize = 6;

struct Schema {
    defs: Map<String, Value>,
}

impl Schema {
    fn new() -> Self {
        let document = document();
        Self {
            defs: document["$defs"].as_object().unwrap().clone(),
        }
    }

    fn def(&self, name: &str) -> &Value {
        self.defs
            .get(name)
            .unwrap_or_else(|| panic!("no $defs entry {name}"))
    }

    /// The branches of the enum `name`: one schema per variant.
    fn variants(&self, name: &str) -> Vec<Value> {
        let def = self.def(name);
        if let Some(branches) = def.get("oneOf").or_else(|| def.get("anyOf")) {
            return branches.as_array().unwrap().clone();
        }
        def["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} is not an enum: {def}"))
            .iter()
            .map(|value| json!({ "const": value }))
            .collect()
    }

    /// One instance of `schema`, optional fields filled down to
    /// [`FILLED_DEPTH`].
    fn instance(&self, schema: &Value, depth: usize) -> Value {
        let filled = depth < FILLED_DEPTH;
        if let Some(target) = schema.get("$ref").and_then(Value::as_str) {
            let name = target.trim_start_matches("#/$defs/");
            return self.instance(self.def(name), depth + 1);
        }
        if let Some(value) = schema.get("const") {
            return value.clone();
        }
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return values[0].clone();
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(branches) = schema.get(key).and_then(Value::as_array) {
                let null = branches.iter().find(|branch| branch["type"] == "null");
                let other = branches.iter().find(|branch| branch["type"] != "null");
                let chosen = match (filled, other, null) {
                    (true, Some(branch), _) | (false, Some(branch), None) => {
                        self.instance(branch, depth + 1)
                    }
                    _ => Value::Null,
                };
                // A tagged newtype's tag sits beside its content's branches.
                if schema.get("properties").is_some()
                    && let (Value::Object(mut fields), Value::Object(tag)) =
                        (chosen.clone(), self.object(schema, depth))
                {
                    fields.extend(tag);
                    return Value::Object(fields);
                }
                return chosen;
            }
        }
        if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
            // A newtype variant of an internally tagged enum: its content's
            // fields beside the tag.
            let mut merged = Map::new();
            for part in parts {
                if let Value::Object(fields) = self.instance(part, depth + 1) {
                    merged.extend(fields);
                }
            }
            return Value::Object(merged);
        }
        let kinds: Vec<&str> = match schema.get("type") {
            Some(Value::String(kind)) => vec![kind.as_str()],
            Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let kind = if filled {
            kinds.iter().find(|kind| **kind != "null").or(kinds.first())
        } else if kinds.contains(&"null") {
            Some(&"null")
        } else {
            kinds.first()
        };
        match kind.copied() {
            Some("object") => self.object(schema, depth),
            Some("array") => {
                let least = schema["minItems"].as_u64().unwrap_or(0);
                let count = if filled { least.max(1) } else { least };
                if let Some(items) = schema.get("prefixItems").and_then(Value::as_array) {
                    return Value::Array(
                        items
                            .iter()
                            .map(|item| self.instance(item, depth + 1))
                            .collect(),
                    );
                }
                let item = schema.get("items").cloned().unwrap_or(Value::Bool(true));
                Value::Array(
                    (0..count)
                        .map(|_| self.instance(&item, depth + 1))
                        .collect(),
                )
            }
            Some("string") => {
                let least = schema["minLength"].as_u64().unwrap_or(1).max(1);
                Value::String("x".repeat(usize::try_from(least).unwrap()))
            }
            Some("integer") => {
                let least = schema["minimum"].as_i64().unwrap_or(1).max(1);
                json!(least)
            }
            Some("number") => json!(1.5),
            Some("boolean") => Value::Bool(true),
            _ => Value::Null,
        }
    }

    fn object(&self, schema: &Value, depth: usize) -> Value {
        let filled = depth < FILLED_DEPTH;
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut object = Map::new();
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, property) in properties {
                if filled || required.contains(&name.as_str()) {
                    // Image bytes travel as base64.
                    let value = if name == "bytes" && property["type"] == "string" {
                        json!("eA==")
                    } else {
                        self.instance(property, depth + 1)
                    };
                    object.insert(name.clone(), value);
                }
            }
        } else if filled
            && let Some(values) = schema.get("additionalProperties")
            && values.is_object()
        {
            object.insert("key".to_owned(), self.instance(values, depth + 1));
        }
        Value::Object(object)
    }
}

/// The variant's wire name, for a failure's message.
fn kind_of(branch: &Value) -> String {
    branch["properties"]
        .as_object()
        .and_then(|properties| {
            properties
                .values()
                .find_map(|property| property.get("const").and_then(Value::as_str))
        })
        .or_else(|| branch.get("const").and_then(Value::as_str))
        .unwrap_or("?")
        .to_owned()
}

/// Observations whose generated value the type refuses to construct: a
/// snapshot or patch that does not validate. Each is built by hand.
fn built(kind: &str) -> Option<Observation> {
    match kind {
        "presentation_snapshot" => Some(Observation::PresentationSnapshot(
            PresentationSnapshot::empty(1),
        )),
        "transcript_patch" => Some(Observation::TranscriptPatch(TranscriptPatch {
            scope: TranscriptScope::Root,
            from_revision: 1,
            to_revision: 2,
            upserts: Vec::new(),
            deletes: Vec::new(),
        })),
        _ => None,
    }
}

/// `value` as an observation: generated, or built when the type refuses
/// the generated one.
fn observation(kind: &str, value: &Value) -> Result<Observation, String> {
    serde_json::from_value::<Observation>(value.clone())
        .or_else(|error| built(kind).ok_or_else(|| format!("{error}: {value}")))
}

fn host_round_trips(frame: &Host) -> Result<(), String> {
    let line = encode_line(frame).map_err(|error| error.to_string())?;
    let decoded = decode_host_line::<PlakExtension>(&line).map_err(|error| error.to_string())?;
    (decoded == *frame)
        .then_some(())
        .ok_or_else(|| format!("decoded differently: {line}"))
}

fn client_round_trips(frame: &Client) -> Result<(), String> {
    let line = encode_line(frame).map_err(|error| error.to_string())?;
    let decoded = decode_line::<Client>(&line).map_err(|error| error.to_string())?;
    (decoded == *frame)
        .then_some(())
        .ok_or_else(|| format!("decoded differently: {line}"))
}

fn event(observation: Observation) -> Host {
    HostFrame::Event {
        attachment: 1,
        seq: 1,
        class: observation.class(),
        observation,
        blobs: Vec::new(),
    }
}

fn intent_request(intent: Intent) -> Client {
    ClientFrame::Request {
        id: 1,
        op: Op::Intent {
            attachment: 1,
            intent,
            blobs: Vec::new(),
        },
    }
}

/// Every failure, then whether there was any.
fn assert_all(what: &str, failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{what}: {} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Every observation variant, and every one of Plak's extension
/// observations, crosses the wire as an event line.
#[test]
fn every_observation_decodes_from_its_event_line() {
    let schema = Schema::new();
    let mut failures = Vec::new();
    let mut values: Vec<(String, Value)> = schema
        .variants("SessionObservation")
        .iter()
        .map(|branch| (kind_of(branch), schema.instance(branch, 0)))
        .collect();
    values.extend(schema.variants("SurfaceObservation").iter().map(|branch| {
        let body = schema.instance(branch, 1);
        (
            format!("extension {}", kind_of(branch)),
            json!({ "kind": "extension", "body": body }),
        )
    }));
    assert!(values.len() > 60, "{} variants", values.len());
    for (kind, value) in values {
        let checked = observation(&kind, &value)
            .and_then(|observation| host_round_trips(&event(observation)));
        if let Err(error) = checked {
            failures.push(format!("{kind}: {error}"));
        }
    }
    assert_all("observations", &failures);
}

/// Every intent variant, and every one of Plak's extension intents,
/// crosses the wire as a request line.
#[test]
fn every_intent_decodes_from_its_request_line() {
    let schema = Schema::new();
    let mut failures = Vec::new();
    let mut values: Vec<(String, Value)> = schema
        .variants("SessionIntent")
        .iter()
        .map(|branch| (kind_of(branch), schema.instance(branch, 0)))
        .collect();
    values.extend(schema.variants("SurfaceIntent").iter().map(|branch| {
        let body = schema.instance(branch, 1);
        (
            format!("extension {}", kind_of(branch)),
            json!({ "kind": "extension", "body": body }),
        )
    }));
    assert!(values.len() > 40, "{} variants", values.len());
    for (kind, value) in values {
        let checked = serde_json::from_value::<Intent>(value.clone())
            .map_err(|error| format!("{error}: {value}"))
            .and_then(|intent| client_round_trips(&intent_request(intent)));
        if let Err(error) = checked {
            failures.push(format!("{kind}: {error}"));
        }
    }
    assert_all("intents", &failures);
}

/// Every host frame kind and client frame kind, every op, answer and
/// refusal, crosses the wire as a line. An event is the observation test's.
#[test]
fn every_frame_decodes_from_its_line() {
    let schema = Schema::new();
    let mut failures = Vec::new();
    for branch in schema.variants("HostFrame") {
        let kind = kind_of(&branch);
        if kind == "event" {
            continue;
        }
        let value = schema.instance(&branch, 0);
        let checked = serde_json::from_value::<Host>(value.clone())
            .map_err(|error| format!("{error}: {value}"))
            .and_then(|frame| host_round_trips(&frame));
        if let Err(error) = checked {
            failures.push(format!("host {kind}: {error}"));
        }
    }
    for branch in schema.variants("ClientFrame") {
        let kind = kind_of(&branch);
        let value = schema.instance(&branch, 0);
        let checked = serde_json::from_value::<Client>(value.clone())
            .map_err(|error| format!("{error}: {value}"))
            .and_then(|frame| client_round_trips(&frame));
        if let Err(error) = checked {
            failures.push(format!("client {kind}: {error}"));
        }
    }
    for branch in schema.variants("Op") {
        let kind = kind_of(&branch);
        let op = schema.instance(&branch, 0);
        let value = json!({ "frame": "request", "id": 1, "op": op });
        let checked = serde_json::from_value::<Client>(value.clone())
            .map_err(|error| format!("{error}: {value}"))
            .and_then(|frame| client_round_trips(&frame));
        if let Err(error) = checked {
            failures.push(format!("op {kind}: {error}"));
        }
    }
    for (name, wrap) in [("Answer", "ok"), ("RequestRefusal", "refused")] {
        for branch in schema.variants(name) {
            let kind = kind_of(&branch);
            let inner = schema.instance(&branch, 0);
            let value = json!({ wrap: inner });
            let checked = serde_json::from_value::<Outcome>(value.clone())
                .map_err(|error| format!("{error}: {value}"))
                .and_then(|outcome| host_round_trips(&HostFrame::Response { id: 1, outcome }));
            if let Err(error) = checked {
                failures.push(format!("{name} {kind}: {error}"));
            }
        }
    }
    assert_all("frames", &failures);
}
