//! `plakd wire schema`: the supported JSON Schema (draft 2020-12) of this
//! build's wire v2 frames, both directions, with Plak's extension traffic,
//! and the protocol version and binary they belong to. A client checks its
//! own frames against the exact build it speaks to.

use std::collections::BTreeSet;
use std::io::Write;

use plakd_wire_types::v13;
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::Value;

/// The schema document: `client_frame` and `host_frame` reference their
/// definitions under `$defs`.
#[must_use]
pub(crate) fn document() -> Value {
    schema(Some(crate::build_identity::binary_id().to_owned()))
}

fn schema(binary_id: Option<String>) -> Value {
    frame_schema::<v13::ClientFrame, v13::HostFrame>(binary_id)
}

fn frame_schema<C: JsonSchema, H: JsonSchema>(binary_id: Option<String>) -> Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    let client = generator.subschema_for::<C>();
    let host = generator.subschema_for::<H>();
    let mut definitions = generator.take_definitions(true);
    // Schemars may discover definitions before the shared DTOs remove replaced branches.
    let mut reachable = BTreeSet::new();
    visit_schema_refs(client.as_value(), &definitions, &mut reachable);
    visit_schema_refs(host.as_value(), &definitions, &mut reachable);
    definitions.retain(|name, _| reachable.contains(name.as_str()));
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "plakd wire v2",
        "wire_version": v13::WIRE_VERSION,
        "binary_id": binary_id,
        "client_frame": client,
        "host_frame": host,
        "$defs": definitions,
    })
}

pub(crate) fn pin() -> Result<String, serde_json::Error> {
    normalized_pin(schema(None))
}

fn normalized_pin(mut document: Value) -> Result<String, serde_json::Error> {
    if let Some(fields) = document.as_object_mut() {
        fields.remove("binary_id");
    }
    document.sort_all_objects();
    let bytes = serde_json::to_vec(&document)?;
    Ok(plakous::model::sha256_hex(&[&bytes]))
}

fn visit_schema_refs(
    value: &Value,
    definitions: &serde_json::Map<String, Value>,
    visited: &mut BTreeSet<String>,
) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if key == "$ref"
                    && let Some(name) = value.as_str().and_then(|s| s.strip_prefix("#/$defs/"))
                    && visited.insert(name.to_owned())
                    && let Some(definition) = definitions.get(name)
                {
                    visit_schema_refs(definition, definitions, visited);
                } else {
                    visit_schema_refs(value, definitions, visited);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                visit_schema_refs(value, definitions, visited);
            }
        }
        _ => {}
    }
}

/// Prints the serving schema as one pretty JSON document.
///
/// # Errors
///
/// A lost stdout.
pub(crate) fn print(stdout: &mut dyn Write) -> CliResult<ProcessOutcome> {
    let schema = document();
    let text = serde_json::to_string_pretty(&schema)
        .map_err(|_| CliFailure::new("plakd wire schema could not encode its schema"))?;
    writeln!(stdout, "{text}").map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod variant_tests;
