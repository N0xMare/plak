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
fn process_interrupt_subject_is_classified_by_the_host_on_the_wire() {
    let mut item = item("write_stdin", ActivityKind::Tool);
    item.subject = Some("Sent interrupt to process build".into());
    assert_eq!(item.operation(), ToolOperation::InterruptProcess);
    let wire = WireToolItem::from(item);
    assert_eq!(wire.operation, ToolOperation::InterruptProcess);
    assert_eq!(
        serde_json::to_value(wire).unwrap()["operation"],
        "interrupt_process"
    );
}

#[test]
fn canonical_and_native_process_writes_classify_input_bytes_not_subject_prose() {
    for (name, key) in [
        ("process_session_write", "input"),
        ("write_stdin", "chars"),
        ("process_session_write", "chars"),
        ("write_stdin", "input"),
    ] {
        for (input, operation) in [
            ("\u{3}", ToolOperation::InterruptProcess),
            ("hello\n", ToolOperation::InputProcess),
            ("Ctrl-C", ToolOperation::InputProcess),
            ("\u{3}suffix", ToolOperation::InputProcess),
        ] {
            let mut arguments = serde_json::json!({"session_id":1});
            arguments[key] = input.into();
            if name == "process_session_write" {
                arguments["disposition"] = "keep_open".into();
            }
            let mut tool = item(name, ActivityKind::Process);
            // Deliberately misleading legacy prose must not override bytes.
            tool.subject = Some("Sent interrupt to process 1".into());
            tool.detail = Some(format!("{arguments}\n\nprocess output"));
            assert_eq!(tool.operation(), operation, "{name}: {input:?}");
            assert_eq!(WireToolItem::from(tool).operation, operation);
        }
        let mut tool = item(name, ActivityKind::Process);
        tool.subject = Some("1".into());
        tool.detail = Some("Ctrl-C".into());
        let persisted = serde_json::to_string(&tool).unwrap();
        let observation = SessionObservation::Row(TranscriptRow::Tool(tool));
        let frame: HostFrame<Core> = HostFrame::Event {
            attachment: 1,
            seq: 1,
            class: observation.class(),
            observation,
            client_id: None,
            blobs: Vec::new(),
        };
        let encoded = encode_line(&frame).unwrap();
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            value["observation"]["body"]["operation"],
            "interrupt_process"
        );
        let HostFrame::Event {
            observation: SessionObservation::Row(TranscriptRow::Tool(decoded)),
            ..
        } = decode_host_line::<Core>(&encoded).unwrap()
        else {
            panic!("not a tool event")
        };
        assert_eq!(decoded.operation(), ToolOperation::InterruptProcess);
        assert_eq!(serde_json::to_string(&decoded).unwrap(), persisted);
    }
}

#[test]
fn process_read_shapes_remain_reads_and_waits_do_not_become_input() {
    for (name, arguments, operation) in [
        (
            "process_session_read",
            serde_json::json!({"session_id":1}),
            ToolOperation::ReadProcess,
        ),
        (
            "process_session_interrupt",
            serde_json::json!({"session_id":1}),
            ToolOperation::InterruptProcess,
        ),
        (
            "process_session_poll",
            serde_json::json!({"session_id":1,"timeout_ms":0}),
            ToolOperation::ReadProcess,
        ),
        (
            "write_stdin",
            serde_json::json!({"session_id":1,"chars":"","yield_time_ms":0}),
            ToolOperation::WaitProcess,
        ),
        (
            "process_session_write",
            serde_json::json!({"session_id":1,"input":""}),
            ToolOperation::WaitProcess,
        ),
        (
            "process_session_poll",
            serde_json::json!({"session_id":1}),
            ToolOperation::ReadProcess,
        ),
        (
            "write_stdin",
            serde_json::json!({"session_id":1,"chars":"","yield_time_ms":100}),
            ToolOperation::WaitProcess,
        ),
        (
            "BashOutput",
            serde_json::json!({"bash_id":"1","block":false}),
            ToolOperation::ReadProcess,
        ),
        (
            "get_command_or_subagent_output",
            serde_json::json!({"id":"1","timeout_ms":0}),
            ToolOperation::ReadProcess,
        ),
        (
            "exec_command",
            serde_json::json!({"session_id":1,"yield_time_ms":0}),
            ToolOperation::ReadProcess,
        ),
    ] {
        let mut tool = item(name, ActivityKind::Process);
        tool.subject = Some("1".into());
        tool.detail = Some(arguments.to_string());
        assert_eq!(WireToolItem::from(tool).operation, operation, "{name}");
    }
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
