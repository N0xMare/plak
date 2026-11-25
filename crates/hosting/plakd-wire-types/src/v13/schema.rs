use std::collections::BTreeSet;

use schemars::generate::SchemaSettings;
use serde_json::Value;

/// The exact protocol 13 JSON Schema document. Each frame root references
/// the shared definitions under `$defs`. Pass no identity for a portable pin.
#[must_use]
pub fn schema(binary_id: Option<&str>) -> Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    let client = generator.subschema_for::<super::ClientFrame>();
    let host = generator.subschema_for::<super::HostFrame>();
    let mut definitions = generator.take_definitions(true);
    // Schemars may discover definitions before the shared DTOs remove replaced branches.
    let mut reachable = BTreeSet::new();
    visit_schema_refs(client.as_value(), &definitions, &mut reachable);
    visit_schema_refs(host.as_value(), &definitions, &mut reachable);
    definitions.retain(|name, _| reachable.contains(name.as_str()));
    Value::Object(
        [
            (
                "$schema",
                "https://json-schema.org/draft/2020-12/schema".into(),
            ),
            ("title", "plakd wire v2".into()),
            ("wire_version", super::WIRE_VERSION.into()),
            ("binary_id", binary_id.map_or(Value::Null, Value::from)),
            ("client_frame", client.as_value().clone()),
            ("host_frame", host.as_value().clone()),
            ("$defs", Value::Object(definitions)),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect(),
    )
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
