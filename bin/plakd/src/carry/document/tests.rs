use super::*;
use std::fs;

const V13_FIXTURE: &str = include_str!("../../../tests/fixtures/session-export-v13.json");

#[test]
fn current_export_bytes_match_the_frozen_v14_fixture() {
    let expected = include_str!("../../../tests/fixtures/session-export-v14-default.json");
    assert_eq!(
        serde_json::to_vec(&sample_document()).unwrap(),
        expected.trim_end().as_bytes()
    );
}

#[test]
fn previous_export_keeps_the_base_writers_exact_v13_bytes_and_is_importable() {
    let previous = include_str!("../../../tests/fixtures/session-export-v13-default.json");
    let current = serde_json::to_vec(&sample_document()).unwrap();
    let current = String::from_utf8(current).unwrap();
    let as_previous = current.replacen("\"version\":14,", "\"version\":13,", 1);
    assert_eq!(as_previous.as_bytes(), previous.trim_end().as_bytes());
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("previous.json");
    fs::write(&path, previous).unwrap();
    let document = read_document(&path).unwrap();
    assert_eq!(document.version, PREVIOUS_VERSION);
    assert_eq!(document.kind, Some(DocumentKind::PortableSession));
    document.into_import().unwrap();
}

fn document_versions() -> [CarryDocument; 2] {
    [
        sample_document(),
        serde_json::from_str(V13_FIXTURE).unwrap(),
    ]
}

#[test]
fn ultrafast_target_round_trips_only_in_the_current_version() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("ultrafast.json");
    let mut document = sample_document();
    document.binding.target_selection = ProductTargetSelection::NativeUltrafast;
    write_document(&path, &document).unwrap();
    let imported = read_document(&path).unwrap().into_import().unwrap();
    assert_eq!(
        imported.record.target_selection,
        ProductTargetSelection::NativeUltrafast
    );

    document.version = PREVIOUS_VERSION;
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let error = read_document(&path).unwrap_err().to_string();
    assert!(error.contains("$.binding.target_selection"), "{error}");
}

#[test]
fn document_kind_is_closed_and_cannot_authorize_a_move_via_portable_import() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("export.json");
    for kind in ["host_upgrade", "unknown"] {
        let mut value = serde_json::to_value(sample_document()).unwrap();
        value["kind"] = serde_json::json!(kind);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let error = read_document(&path).unwrap_err().to_string();
        assert!(
            error.contains(if kind == "host_upgrade" {
                "host_upgrade is a reserved document kind and cannot be imported"
            } else {
                "unknown variant"
            }),
            "{error}"
        );
    }
}

#[test]
fn every_remedy_round_trips_and_an_unknown_remedy_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("export.json");
    let mut document = sample_document();
    document.transcript = crate::notice_text::tests::all_remedies()
        .into_iter()
        .map(|remedy| {
            TranscriptRow::info(plakous_hosted::sessions::NoticeScope::Host, "neutral facts")
                .with_remedy(remedy)
        })
        .collect();
    let encoded = serde_json::to_value(&document).unwrap();
    fs::write(&path, encoded.to_string()).unwrap();
    assert_eq!(
        read_document(&path)
            .unwrap()
            .into_import()
            .unwrap()
            .record
            .transcript
            .to_vec(),
        document.transcript
    );
    for invalid in [
        serde_json::json!({"code":"future_code"}),
        serde_json::json!({"code":"login","account":"not_an_account"}),
        serde_json::json!({"code":"resume_saved_session","session_id":"invalid"}),
        serde_json::json!({"code":"recover_session","extra":true}),
    ] {
        let mut bad = encoded.clone();
        bad["transcript"][0]["remedy"] = invalid;
        fs::write(&path, bad.to_string()).unwrap();
        assert!(read_document(&path).is_err());
    }
}

#[test]
fn current_and_previous_documents_preserve_every_transcript_row_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    for mut document in document_versions() {
        document.transcript = crate::store::every_stored_row_type();
        let value = serde_json::to_value(&document).unwrap();
        fs::write(&path, value.to_string()).unwrap();
        let imported = read_document(&path).unwrap().into_import().unwrap();
        assert_eq!(imported.record.transcript.to_vec(), document.transcript);
    }
}

#[test]
fn carry_files_are_private_and_never_overwritten() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(root.path().join("home"));
    fs::create_dir(paths.home()).unwrap();
    let path =
        crate::carry::new_carry_file(&paths, "5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11").unwrap();
    let dir = path.parent().unwrap();
    assert_eq!(dir, paths.home().join("carry"));
    assert_eq!(
        fs::metadata(dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let document = sample_document();
    write_document(&path, &document).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(write_document(&path, &document).is_err());
    let read = read_document(&path).unwrap();
    assert_eq!(read.source.session_id, document.source.session_id);
}

#[test]
fn current_and_previous_documents_carry_send_times_and_process_commands() {
    use plakous_host_protocol::views::transcript::{ProcessNotice, ProcessOutcome};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    for mut document in document_versions() {
        document.transcript = vec![
            TranscriptRow::User {
                text: "first".into(),
                sent_at_ms: Some(1_791_383_520_000),
            },
            TranscriptRow::Process(ProcessNotice {
                job_id: "p261".into(),
                outcome: ProcessOutcome::Exited { code: Some(0) },
                duration_ms: Some(1_300_000),
                subject: Some("just check".into()),
            }),
        ];
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(value["transcript"][0]["sent_at_ms"], 1_791_383_520_000_u64);
        assert_eq!(value["transcript"][1]["subject"], "just check");
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            read_document(&path).unwrap().transcript,
            document.transcript
        );
    }
}

#[test]
fn current_and_previous_documents_reject_resolved_count_pseudo_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    for document in document_versions() {
        let mut value = serde_json::to_value(document).unwrap();
        value["pending_work"] = serde_json::json!([{
            "operation": "resolved_source_work", "kind": "source_resolved_count",
            "state": "3", "detail": "settled child/candidate/workspace rows omitted"
        }]);
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            read_document(&path).unwrap_err().to_string(),
            "pending work cannot contain source_resolved_count; use the typed field"
        );
    }
}

#[test]
fn v8_is_refused_before_import_regardless_of_named_state_or_selection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    let original: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/session-export-v8.json"
    ))
    .unwrap();
    for state in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!({"answer":42}),
    ] {
        let mut value = original.clone();
        value["named_state"] = state;
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            read_document(&path).unwrap_err().to_string(),
            format!(
                "{} is a version 8 session export; this build imports versions 13 and 14",
                path.display()
            )
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    let mut value = original;
    value["binding"]["composition"] = serde_json::json!("py-sdk");
    fs::write(&path, value.to_string()).unwrap();
    assert_eq!(
        read_document(&path).unwrap_err().to_string(),
        format!(
            "{} is a version 8 session export; this build imports versions 13 and 14",
            path.display()
        )
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

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
    assert_eq!(
        (&published.source.session_id, &published.source.state_key),
        (&document.source.session_id, &document.source.state_key)
    );
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
fn file_import_validates_named_state_keys_count_and_total_before_creation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("export.json");
    for state in [
        std::collections::BTreeMap::from([("invalid key".into(), serde_json::json!(1))]),
        (0..65)
            .map(|n| (format!("key{n}"), serde_json::json!(n)))
            .collect(),
        (0..5)
            .map(|n| (format!("key{n}"), serde_json::json!("x".repeat(900_000))))
            .collect(),
    ] {
        let mut document = sample_document();
        document.named_state = state;
        fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        assert!(read_document(&path).unwrap().into_import().is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

#[test]
fn v13_import_preserves_named_state_and_v12_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let mut value: serde_json::Value = serde_json::from_str(V13_FIXTURE).unwrap();
    value["named_state"] = serde_json::json!({"answer":42});
    let path = root.path().join("v13.json");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let imported = read_document(&path).unwrap().into_import().unwrap();
    assert_eq!(imported.history.named_state().unwrap()["answer"], 42);
    fs::write(
        &path,
        include_str!("../../../tests/fixtures/session-export-v12-default.json"),
    )
    .unwrap();
    let error = read_document(&path).unwrap_err().to_string();
    assert!(
        error.contains("version 12 session export; this build imports versions 13 and 14"),
        "{error}"
    );
}

/// Previous-version history and saved placement/profile survive the version cut.
#[test]
fn a_frozen_v13_document_imports_with_an_identical_canonical_history() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("v13.json");
    fs::write(&path, V13_FIXTURE).unwrap();
    let document = read_document(&path).unwrap();
    assert_eq!(document.version, PREVIOUS_VERSION);
    assert_eq!(document.kind, Some(DocumentKind::PortableSession));
    assert!(document.runner.is_some());
    assert!(document.profile.is_some());
    let import = document.into_import_with_replay(true).unwrap();
    assert_eq!(
        import.profile.as_ref().unwrap().root.as_deref(),
        Some("lead")
    );
    assert_eq!(
        import.record.runner, None,
        "the local runner is the record default"
    );
    let mut fixture: serde_json::Value = serde_json::from_str(V13_FIXTURE).unwrap();
    let reexport = |history| serde_json::to_value(history::History::export(history).unwrap());
    assert_eq!(reexport(&import.history).unwrap(), fixture["history"]);
    assert_eq!(import.history.segments().len(), 4);
    // A file import keeps everything but the source account's replay scope.
    let file = read_document(&path).unwrap().into_import().unwrap();
    fixture["history"]["replay_account"] = "".into();
    assert_eq!(reexport(&file.history).unwrap(), fixture["history"]);
    assert_eq!(
        import.record.carried_forward_from(),
        Some(("b1c6cbe1-3c06-4c80-bbb3-0b670b2a8fd0", "r26.e34.p35.t9"))
    );
}

#[test]
fn v14_names_its_kind_runner_and_profile_and_round_trips() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("v14.json");
    let document = sample_document();
    write_document(&path, &document).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["version"], 14);
    assert_eq!(value["kind"], "portable_session");
    assert_eq!(
        value["runner"],
        serde_json::json!({"name":"local","runner_id":"local"})
    );
    assert_eq!(
        value["profile"],
        serde_json::json!({"root_template":"lead","working_set":["review"]})
    );
    let read = read_document(&path).unwrap();
    assert_eq!(serde_json::to_value(&read).unwrap(), value);
    let import = read.into_import().unwrap();
    assert_eq!(
        import.profile,
        Some(crate::profiles::SessionSelection {
            root: Some("lead".to_owned()),
            working_set: vec!["review".to_owned()],
        })
    );
}

type Mutation = fn(&mut serde_json::Value);

/// Each refusal names the field, the version and the JSON path.
#[test]
fn malformed_v14_documents_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("export.json");
    let valid = serde_json::to_value(sample_document()).unwrap();
    let mutations: [(&str, Mutation); 10] = [
        ("version 14 requires `runner` at $.runner", |v| {
            v.as_object_mut().unwrap().remove("runner");
        }),
        ("version 14 requires `profile` at $.profile", |v| {
            v.as_object_mut().unwrap().remove("profile");
        }),
        ("version 14 requires `runner` at $.runner", |v| {
            v["runner"] = serde_json::Value::Null;
        }),
        ("export kind does not match its version", |v| {
            v.as_object_mut().unwrap().remove("kind");
        }),
        ("version 12 session export", |v| {
            v["version"] = 12.into();
        }),
        (
            "$.runner: export runner is not a runner name and identity",
            |v| {
                v["runner"]["runner_id"] = "".into();
            },
        ),
        (
            "$.runner: export runner is not a runner name and identity",
            |v| {
                v["runner"]["name"] = "two words".into();
            },
        ),
        ("unknown field `host`", |v| v["runner"]["host"] = "x".into()),
        ("$.profile: root profile names an invalid template", |v| {
            v["profile"]["root_template"] = "not a template".into();
        }),
        ("$.profile: root profile lists a template twice", |v| {
            v["profile"]["working_set"] = serde_json::json!(["review", "review"]);
        }),
    ];
    for (expected, mutate) in mutations {
        let mut value = valid.clone();
        mutate(&mut value);
        fs::write(&path, value.to_string()).unwrap();
        let error = read_document(&path)
            .and_then(CarryDocument::into_import)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{expected}: {error}");
    }
    let mut remote = valid;
    remote["runner"] = serde_json::json!({"name":"build-box","runner_id":"build-box"});
    fs::write(&path, remote.to_string()).unwrap();
    let imported = read_document(&path).unwrap().into_import().unwrap();
    let runner = imported.record.runner.unwrap();
    assert_eq!(runner.runner_id, "build-box");
    assert_eq!(
        runner.workspace_root,
        imported.record.workspace_root.display().to_string()
    );
}

#[test]
fn v14_document_round_trips_named_state_selection_and_full_history() {
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
    document.binding.composition = ProductCompositionSelection::PySdk;
    document
        .named_state
        .insert("answer".into(), serde_json::json!({"n":42}));
    document.history = history::History::export(&original).unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("v14.json");
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

#[test]
fn rows_sharing_a_call_id_survive_a_document_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    // A carried session's imported result, then its own tree's first result
    // under the same call id: rows are identified by position, not call id.
    let result = |subject: &str| {
        TranscriptRow::Tool(plakous_hosted::sessions::ToolItem {
            wire_operation: None,
            call_id: "child-result:m1".into(),
            name: "child_result".into(),
            exposed_name: None,
            kind: plakous_host_protocol::views::ActivityKind::Agent,
            state: "done".into(),
            subject: Some(subject.into()),
            detail: None,
            started_at_ms: None,
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        })
    };
    for mut document in document_versions() {
        document.transcript = vec![
            TranscriptRow::user("before the carry"),
            result("m1 · c1 completed"),
            TranscriptRow::assistant("carried answer"),
            result("m1 · c2 completed"),
        ];
        fs::write(&path, serde_json::to_value(&document).unwrap().to_string()).unwrap();
        let imported = read_document(&path).unwrap().into_import().unwrap();
        assert_eq!(imported.record.transcript.to_vec(), document.transcript);
    }
}
