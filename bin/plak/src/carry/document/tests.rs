use super::*;
use std::fs;

#[test]
fn published_export_survives_directory_sync_failure() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("export.json");
    let document = sample_document();
    write_document_with_directory_sync(&path, &document, |_| {
        Err(std::io::Error::other("synthetic directory sync failure"))
    })
    .unwrap();
    let published = read_document(&path).unwrap();
    assert_eq!(published.source(), document.source());
    assert!(write_document(&path, &document).is_err());
}

#[test]
fn oversized_named_state_is_refused_before_import() {
    let mut document = sample_document();
    document
        .named_state
        .insert("large".into(), serde_json::json!("x".repeat(1_048_576)));
    assert!(document.into_import().is_err());
}

#[test]
fn v8_import_starts_with_empty_named_state_and_v7_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let mut value = serde_json::to_value(sample_document()).unwrap();
    value["version"] = serde_json::json!(8);
    value.as_object_mut().unwrap().remove("named_state");
    let path = root.path().join("v8.json");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let imported = read_document(&path).unwrap().into_import().unwrap();
    assert!(imported.history.named_state().unwrap().is_empty());
    value["version"] = serde_json::json!(7);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let error = read_document(&path).unwrap_err().to_string();
    assert!(
        error.contains("version 7 session export; this build imports versions 8 and 9"),
        "{error}"
    );
}

#[test]
fn v9_document_round_trips_named_state_selection_and_full_history() {
    use plakous::model::{
        Actor, ContentBlock, ConversationItem, EntryId, HistorySegment, HistorySegmentKind,
        ModelId, Provenance, TextBlock,
    };
    use plakous::sessions::PortableSessionHistory;
    let item = ConversationItem::new(
        EntryId::try_from(uuid::Uuid::new_v4()).unwrap(),
        Actor::User,
        vec![ContentBlock::Text(
            TextBlock::new("hello world!".to_owned(), None).unwrap(),
        )],
        Provenance::UserInput,
    )
    .unwrap();
    let original = PortableSessionHistory::new(
        vec![HistorySegment::new(HistorySegmentKind::RecentIntent, vec![item]).unwrap()],
        "00ff".into(),
        ModelId::new("carry.model").unwrap(),
    );
    let mut document = sample_document();
    document.version = VERSION;
    document.binding.composition = ProductCompositionSelection::PySdk;
    document
        .named_state
        .insert("answer".into(), serde_json::json!({"n":42}));
    document.history = Some(history::History::export(&original).unwrap());
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("v9.json");
    write_document(&path, &document).unwrap();
    let bytes = fs::read(&path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value.get("checkpoint").is_none());
    assert!(value.get("archive").is_none());
    for field in ["checkpoint", "archive"] {
        let mut malformed = value.clone();
        malformed[field] = serde_json::Value::Null;
        let rejected = root.path().join(format!("{field}.json"));
        fs::write(&rejected, serde_json::to_vec(&malformed).unwrap()).unwrap();
        assert!(read_document(&rejected).is_err());
    }
    let document = read_document(&path).unwrap();
    assert_eq!(
        document.carry_summary(),
        "1 messages, ~3 tokens; the first request rebuilds the prompt cache"
    );
    let import = document.into_import().unwrap();
    assert_eq!(import.history.segments(), original.segments());
    assert_eq!(
        import.record.composition,
        ProductCompositionSelection::PySdk
    );
    assert_eq!(
        import.history.named_state().unwrap()["answer"],
        serde_json::json!({"n":42})
    );
    assert!(
        import
            .note
            .contains("first request rebuilds the prompt cache")
    );
}
