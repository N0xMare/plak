use super::*;
use crate::views::{ToolOperation, presentation::ActivityKind};

#[test]
fn wire_operation_is_process_local_and_does_not_change_exported_row_bytes_or_equality() {
    let golden = r#"{"kind":"tool","call_id":"call","name":"unknown","activity_kind":"tool","state":"done"}"#;
    let plain: TranscriptRow = serde_json::from_str(golden).unwrap();
    let mut projected = plain.clone();
    let TranscriptRow::Tool(item) = &mut projected else {
        panic!("tool row")
    };
    assert_eq!(item.operation(), ToolOperation::Generic);
    item.wire_operation = Some(ToolOperation::ReadProcess);
    assert_eq!(item.operation(), ToolOperation::ReadProcess);
    assert_eq!(projected, plain);
    assert_eq!(serde_json::to_string(&plain).unwrap(), golden);
    assert_eq!(serde_json::to_string(&projected).unwrap(), golden);
    let restored: TranscriptRow = serde_json::from_str(golden).unwrap();
    let TranscriptRow::Tool(restored) = restored else {
        panic!("tool row")
    };
    assert_eq!(restored.wire_operation, None);
    assert_eq!(
        restored.operation(),
        ToolOperation::classify("unknown", ActivityKind::Tool, None)
    );
    let schema = serde_json::to_value(schemars::schema_for!(ToolItem)).unwrap();
    assert!(schema["properties"].get("wire_operation").is_none());
    assert!(schema["properties"].get("operation").is_none());
}
