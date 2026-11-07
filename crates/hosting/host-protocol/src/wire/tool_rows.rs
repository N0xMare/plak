//! Tool-operation projection at the framed boundary, never at store/export serde sites.

use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::WireToolItem;
use crate::views::ToolOperation;
use crate::views::transcript::{ToolItem, TranscriptItem, TranscriptRow, TranscriptStreamDelta};
use crate::{ProtocolExtension, SessionObservation};

fn overlay(value: &Value) -> Option<ToolOperation> {
    value
        .get("operation")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn item_overlays(value: &Value) -> Vec<Option<ToolOperation>> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| item.get("row").and_then(overlay))
        .collect()
}

fn restore_items(items: &mut [TranscriptItem], overlays: Vec<Option<ToolOperation>>) {
    for (item, operation) in items.iter_mut().zip(overlays) {
        if let TranscriptRow::Tool(tool) = &mut item.row {
            tool.wire_operation = operation;
        }
    }
}

fn tool(value: &mut Value) -> Result<(), serde_json::Error> {
    let item: WireToolItem = serde_json::from_value(value.take())?;
    *value = serde_json::to_value(ToolItem::from(item))?;
    Ok(())
}

fn row(value: &mut Value) -> Result<(), serde_json::Error> {
    if value.get("kind").and_then(Value::as_str) != Some("tool") {
        return Ok(());
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("kind");
    }
    tool(value)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("kind".to_owned(), Value::String("tool".to_owned()));
    }
    Ok(())
}

fn items(value: &mut Value) -> Result<(), serde_json::Error> {
    if let Some(items) = value.as_array_mut() {
        for item in items {
            if let Some(value) = item.get_mut("row") {
                row(value)?;
            }
        }
    }
    Ok(())
}

fn observation(value: &mut Value) -> Result<(), serde_json::Error> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let Some(body) = value.get_mut("body") else {
        return Ok(());
    };
    match kind.as_str() {
        "row" => row(body)?,
        "presentation_snapshot" => {
            if let Some(value) = body.get_mut("items") {
                items(value)?;
            }
        }
        "transcript_patch" => {
            if let Some(value) = body.get_mut("upserts") {
                items(value)?;
            }
        }
        "transcript_stream" => {
            if let Some(value) = body
                .get_mut("delta")
                .and_then(|delta| delta.get_mut("tool"))
            {
                tool(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn encode_row(value: &mut Value, row: &TranscriptRow) -> Result<(), serde_json::Error> {
    if let TranscriptRow::Tool(tool) = row {
        *value = serde_json::to_value(WireToolItem::from(tool.clone()))?;
        if let Some(object) = value.as_object_mut() {
            object.insert("kind".to_owned(), Value::String("tool".to_owned()));
        }
    }
    Ok(())
}

fn encode_items(value: &mut Value, items: &[TranscriptItem]) -> Result<(), serde_json::Error> {
    if let Some(encoded) = value.as_array_mut() {
        for (encoded, item) in encoded.iter_mut().zip(items) {
            encode_row(&mut encoded["row"], &item.row)?;
        }
    }
    Ok(())
}

pub(super) fn serialize_observation<S: serde::Serializer, P: ProtocolExtension>(
    value: &SessionObservation<P>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    P::Intent: Serialize,
    P::Observation: Serialize,
{
    let mut encoded = serde_json::to_value(value).map_err(serde::ser::Error::custom)?;
    let project = |encoded: &mut Value| -> Result<(), serde_json::Error> {
        match value {
            SessionObservation::Row(row) => encode_row(&mut encoded["body"], row)?,
            SessionObservation::PresentationSnapshot(snapshot) => {
                encode_items(&mut encoded["body"]["items"], &snapshot.items)?;
            }
            SessionObservation::TranscriptPatch(patch) => {
                encode_items(&mut encoded["body"]["upserts"], &patch.upserts)?;
            }
            SessionObservation::TranscriptStream {
                delta: TranscriptStreamDelta::Tool(tool),
                ..
            } => {
                encoded["body"]["delta"]["tool"] =
                    serde_json::to_value(WireToolItem::from(tool.as_ref().clone()))?;
            }
            _ => {}
        }
        Ok(())
    };
    project(&mut encoded).map_err(serde::ser::Error::custom)?;
    encoded.serialize(serializer)
}

pub(super) fn deserialize_observation<'de, D: serde::Deserializer<'de>, P: ProtocolExtension>(
    deserializer: D,
) -> Result<SessionObservation<P>, D::Error>
where
    P::Intent: DeserializeOwned,
    P::Observation: DeserializeOwned,
{
    let mut value = Value::deserialize(deserializer)?;
    let single = value
        .pointer("/body")
        .and_then(overlay)
        .or_else(|| value.pointer("/body/delta/tool").and_then(overlay));
    let overlays = value
        .pointer("/body/items")
        .or_else(|| value.pointer("/body/upserts"))
        .map(item_overlays)
        .unwrap_or_default();
    observation(&mut value).map_err(serde::de::Error::custom)?;
    let mut decoded = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
    match &mut decoded {
        SessionObservation::Row(TranscriptRow::Tool(tool)) => tool.wire_operation = single,
        SessionObservation::TranscriptStream {
            delta: TranscriptStreamDelta::Tool(tool),
            ..
        } => tool.wire_operation = single,
        SessionObservation::PresentationSnapshot(snapshot) => {
            restore_items(&mut snapshot.items, overlays);
        }
        SessionObservation::TranscriptPatch(patch) => restore_items(&mut patch.upserts, overlays),
        _ => {}
    }
    Ok(decoded)
}

pub(super) fn serialize_items<S: serde::Serializer>(
    value: &[TranscriptItem],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut encoded = serde_json::to_value(value).map_err(serde::ser::Error::custom)?;
    encode_items(&mut encoded, value).map_err(serde::ser::Error::custom)?;
    encoded.serialize(serializer)
}

pub(super) fn deserialize_items<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<TranscriptItem>, D::Error> {
    let mut value = Value::deserialize(deserializer)?;
    let overlays = item_overlays(&value);
    items(&mut value).map_err(serde::de::Error::custom)?;
    let mut decoded =
        serde_json::from_value::<Vec<TranscriptItem>>(value).map_err(serde::de::Error::custom)?;
    restore_items(&mut decoded, overlays);
    Ok(decoded)
}

fn replace_reference(value: &mut Value, from: &Value, to: &Value) {
    match value {
        Value::Object(object) => {
            if object.get("$ref") == Some(from) {
                object.insert("$ref".to_owned(), to.clone());
            }
            for value in object.values_mut() {
                replace_reference(value, from, to);
            }
        }
        Value::Array(values) => {
            for value in values {
                replace_reference(value, from, to);
            }
        }
        _ => {}
    }
}

fn project_inline_tool_schema(value: &mut Value, operation: &Value) {
    match value {
        Value::Object(object) => {
            let tool = object.get("properties").is_some_and(|properties| {
                properties.pointer("/kind/const").and_then(Value::as_str) == Some("tool")
                    && properties.get("call_id").is_some()
                    && properties.get("activity_kind").is_some()
            });
            if tool {
                if let Some(properties) =
                    object.get_mut("properties").and_then(Value::as_object_mut)
                {
                    properties.insert("operation".to_owned(), operation.clone());
                }
                if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut)
                    && !required.iter().any(|field| field == "operation")
                {
                    required.push(Value::String("operation".to_owned()));
                }
            }
            for value in object.values_mut() {
                project_inline_tool_schema(value, operation);
            }
        }
        Value::Array(values) => {
            for value in values {
                project_inline_tool_schema(value, operation);
            }
        }
        _ => {}
    }
}

fn schema<T: schemars::JsonSchema>(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = T::json_schema(generator);
    let stored = generator.subschema_for::<ToolItem>();
    let wire = generator.subschema_for::<WireToolItem>();
    let operation = generator.subschema_for::<ToolOperation>().to_value();
    if let (Some(from), Some(to)) = (stored.get("$ref"), wire.get("$ref")) {
        if let Some(object) = schema.as_object_mut() {
            for value in object.values_mut() {
                replace_reference(value, from, to);
                project_inline_tool_schema(value, &operation);
            }
        }
        for value in generator.definitions_mut().values_mut() {
            replace_reference(value, from, to);
            project_inline_tool_schema(value, &operation);
        }
    }
    schema
}

pub(super) struct ObservationSchema<P>(PhantomData<P>);

impl<P: ProtocolExtension> schemars::JsonSchema for ObservationSchema<P>
where
    P::Intent: schemars::JsonSchema,
    P::Observation: schemars::JsonSchema,
{
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "WireSessionObservation".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schema::<SessionObservation<P>>(generator)
    }
}

pub(super) struct ItemsSchema;

impl schemars::JsonSchema for ItemsSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "WireTranscriptItems".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schema::<Vec<TranscriptItem>>(generator)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
