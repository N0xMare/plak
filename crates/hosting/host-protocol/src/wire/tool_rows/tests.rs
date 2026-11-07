use super::*;
use crate::Core;
use crate::views::transcript::{
    Lane, RowKey, TranscriptPatch, TranscriptRow, TranscriptScope, TranscriptStreamDelta,
};
use crate::views::{PresentationSnapshot, ToolOperation, presentation::ActivityKind};
use crate::wire::{HostFrame, SnapshotPage, decode_host_line, encode_line};

fn item(name: &str, kind: ActivityKind) -> ToolItem {
    ToolItem {
        wire_operation: None,
        call_id: "call-1".into(),
        name: name.into(),
        exposed_name: None,
        kind,
        state: "running".into(),
        subject: None,
        detail: None,
        started_at_ms: None,
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: None,
        wait: None,
    }
}

fn row(item: ToolItem) -> TranscriptItem {
    TranscriptItem {
        key: RowKey::new(1, Lane::Runtime, 0),
        row: TranscriptRow::Tool(item),
    }
}

#[test]
fn tool_projection_never_changes_stored_or_exported_row_bytes() {
    let item = item("apply_patch", ActivityKind::Tool);
    let persisted = serde_json::to_string(&TranscriptRow::Tool(item.clone())).unwrap();
    assert_eq!(
        persisted,
        r#"{"kind":"tool","call_id":"call-1","name":"apply_patch","activity_kind":"tool","state":"running"}"#
    );
    let wire = WireToolItem::from(item.clone());
    assert_eq!(wire.operation, ToolOperation::Edit);
    assert_eq!(serde_json::to_value(&wire).unwrap()["operation"], "edit");
    assert_eq!(
        serde_json::to_string(&TranscriptRow::Tool(ToolItem::from(wire))).unwrap(),
        persisted
    );
    assert_eq!(
        serde_json::to_string(&TranscriptRow::Tool(item)).unwrap(),
        persisted
    );
}

#[test]
fn snapshots_patches_rows_and_streams_all_carry_the_host_operation() {
    for (name, kind, operation) in [
        ("apply_patch", ActivityKind::Tool, "edit"),
        ("mcp__docs__read", ActivityKind::Tool, "mcp"),
        ("composition", ActivityKind::Compose, "compose"),
        ("unknown_tool", ActivityKind::Tool, "generic"),
    ] {
        let item = item(name, kind);
        let observations: [(SessionObservation<Core>, &str); 4] = [
            (
                SessionObservation::Row(TranscriptRow::Tool(item.clone())),
                "/observation/body/operation",
            ),
            (
                SessionObservation::PresentationSnapshot(PresentationSnapshot {
                    schema: crate::views::presentation::PRESENTATION_SCHEMA_V3,
                    scope: TranscriptScope::Root,
                    revision: 1,
                    items: vec![row(item.clone())],
                }),
                "/observation/body/items/0/row/operation",
            ),
            (
                SessionObservation::TranscriptPatch(TranscriptPatch {
                    scope: TranscriptScope::Root,
                    from_revision: 0,
                    to_revision: 1,
                    upserts: vec![row(item.clone())],
                    deletes: Vec::new(),
                }),
                "/observation/body/upserts/0/row/operation",
            ),
            (
                SessionObservation::TranscriptStream {
                    scope: TranscriptScope::Root,
                    delta: TranscriptStreamDelta::Tool(Box::new(item.clone())),
                },
                "/observation/body/delta/tool/operation",
            ),
        ];
        for (observation, pointer) in observations {
            let frame = HostFrame::Event {
                attachment: 1,
                seq: 1,
                class: observation.class(),
                observation,
                client_id: None,
                blobs: Vec::new(),
            };
            let line = encode_line(&frame).unwrap();
            let mut value: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(value.pointer(pointer).unwrap(), operation);
            assert_eq!(decode_host_line::<Core>(&line).unwrap(), frame);
            // The wire is authoritative even for a verb today's classifier
            // never emits. Decode must not erase it through shared-row serde.
            *value.pointer_mut(pointer).unwrap() = Value::String("read_process".into());
            let decoded = decode_host_line::<Core>(&value.to_string()).unwrap();
            let round_trip: Value = serde_json::from_str(&encode_line(&decoded).unwrap()).unwrap();
            assert_eq!(round_trip.pointer(pointer).unwrap(), "read_process");
            *value.pointer_mut(pointer).unwrap() = Value::String("not_an_operation".into());
            assert!(decode_host_line::<Core>(&value.to_string()).is_err());
        }
        let page: HostFrame<Core> = HostFrame::Snapshot(SnapshotPage {
            attachment: 1,
            schema: crate::views::presentation::PRESENTATION_SCHEMA_V3,
            scope: TranscriptScope::Root,
            revision: 1,
            page: 0,
            items: vec![row(item)],
            last: true,
        });
        let line = encode_line(&page).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["items"][0]["row"]["operation"],
            operation
        );
        assert_eq!(decode_host_line::<Core>(&line).unwrap(), page);
    }
}

#[test]
fn wire_schema_requires_operation_but_shared_tool_schema_does_not() {
    fn check_rows(value: &Value, count: &mut usize) {
        match value {
            Value::Object(object) => {
                if value
                    .pointer("/properties/kind/const")
                    .and_then(Value::as_str)
                    == Some("tool")
                {
                    *count += 1;
                    if value.pointer("/properties/call_id").is_some() {
                        assert!(value.pointer("/properties/operation").is_some(), "{value}");
                        assert!(
                            value["required"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|field| field == "operation")
                        );
                    } else {
                        assert!(value.to_string().contains("WireToolItem"), "{value}");
                    }
                }
                for child in object.values() {
                    check_rows(child, count);
                }
            }
            Value::Array(array) => {
                for child in array {
                    check_rows(child, count);
                }
            }
            _ => {}
        }
    }
    let schema = serde_json::to_value(schemars::schema_for!(HostFrame<Core>)).unwrap();
    let wire = &schema["$defs"]["WireToolItem"];
    assert!(
        wire["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "operation")
    );
    assert_eq!(wire["additionalProperties"], false);
    let mut rows = 0;
    check_rows(&schema, &mut rows);
    assert!(rows > 0);
    let stored = serde_json::to_value(schemars::schema_for!(ToolItem)).unwrap();
    assert!(stored["properties"].get("operation").is_none());
}
