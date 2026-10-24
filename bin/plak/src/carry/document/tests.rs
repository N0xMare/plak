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
fn oversized_checkpoints_and_archives_refuse_with_typed_errors() {
    let mut document = sample_document();
    document.checkpoint.as_mut().unwrap().user_intent = "x".repeat(MAX_CHECKPOINT_BYTES + 1);
    assert!(matches!(
        document.into_import(),
        Err(ImportError::Oversized {
            section: "checkpoint",
            ..
        })
    ));
    let mut document = sample_document();
    document.archive.as_mut().unwrap().entries[0].text = "x".repeat(MAX_ARCHIVE_BYTES + 1);
    assert!(matches!(
        document.into_import(),
        Err(ImportError::Oversized {
            section: "archive",
            ..
        })
    ));
}

#[test]
fn previous_checkpoint_halves_have_independent_ceilings() {
    let root = tempfile::tempdir().unwrap();
    for reference in [false, true] {
        let mut document = sample_document();
        document.checkpoint.as_mut().unwrap().user_intent.clear();
        document.checkpoint.as_mut().unwrap().reference.clear();
        let text = "x".repeat(MAX_CHECKPOINT_BYTES / 2 + 1);
        if reference {
            document.checkpoint.as_mut().unwrap().reference = text;
        } else {
            document.checkpoint.as_mut().unwrap().user_intent = text;
        }
        let path = root.path().join(format!("{reference}.json"));
        write_document(&path, &document).unwrap();
        assert_eq!(
            read_document(&path).unwrap_err(),
            ImportError::Oversized {
                section: if reference {
                    "checkpoint reference"
                } else {
                    "checkpoint user intent"
                },
                bytes: MAX_CHECKPOINT_BYTES / 2 + 1,
                maximum: MAX_CHECKPOINT_BYTES / 2,
            }
        );
    }
}

#[test]
fn v8_document_round_trips_full_history_and_confirms_cache_rebuild() {
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
    document.checkpoint = None;
    document.archive = None;
    document.history = Some(history::History::export(&original).unwrap());
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("v8.json");
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
    assert!(import.archive.is_none());
    assert!(
        import
            .note
            .contains("first request rebuilds the prompt cache")
    );
}
