use super::document::{ImportError, sample_document};
use super::export::ExportRefusal;
use super::*;
use crate::home::paths::PlakPaths;
use crate::store::sessions::{QueueDrainState, SessionRecord};
use plakous_hosted::sessions::{MAX_TRANSCRIPT_LINES, ThinkingRow, ToolItem, TranscriptRow};
use std::fs;

#[test]
fn named_api_v12_import_refuses_even_with_a_default_key_present() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home")).for_workspace(root.path());
    paths.ensure_layout().unwrap();
    fs::write(
        paths.home().join("secrets/openai.key"),
        "unused-default-key",
    )
    .unwrap();
    for account in ["openai/work", "unknown"] {
        let services = crate::shell::services::Services::for_home(paths.clone());
        let mut document = sample_document();
        document.workspace_root = root.path().to_owned();
        document.binding.account = Some(account.into());
        document.binding.target_selection =
            crate::product_config::ProductTargetSelection::Platform {
                provider: plakous_hosted::binding::PlatformProvider::OpenAi,
            };
        let path = root.path().join("export.json");
        fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let failure =
            crate::commands::session::import_export::import_session(&services, &path).unwrap_err();
        if account == "unknown" {
            assert_eq!(services.product_store_opens(), 0);
            assert_eq!(failure.exit_code(), 2);
            assert!(matches!(
                std::error::Error::source(&failure).unwrap()
                    .downcast_ref::<plakous_hosted::capabilities::AccountAdmissionError>(),
                Some(plakous_hosted::capabilities::AccountAdmissionError::UnknownAccount { account: refused })
                    if refused == account
            ));
        } else {
            assert_eq!(failure.exit_code(), 3, "{failure}");
            assert!(failure.message().contains("openai/work"), "{failure}");
            assert!(!failure.message().contains("unused-default-key"));
            assert_eq!(
                services
                    .session_records()
                    .unwrap()
                    .list_for_workspace(root.path())
                    .unwrap(),
                [] as [SessionRecord; 0]
            );
        }
    }
}

#[test]
fn import_and_resume_refuse_account_target_mismatches_before_credentials() {
    use plakous_hosted::binding::{PlatformProvider, TargetSelection};
    use plakous_hosted::capabilities::AccountAdmissionError;
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home")).for_workspace(root.path());
    paths.ensure_layout().unwrap();
    let services = crate::shell::services::Services::for_home(paths);
    for account in ["openai", "openai/work", "claude", "openrouter/work"] {
        let mut document = sample_document();
        document.binding.account = Some(account.into());
        document.binding.target_selection = TargetSelection::Platform {
            provider: PlatformProvider::Anthropic,
        };
        let path = root.path().join("export.json");
        fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let imported =
            crate::commands::session::import_export::import_session(&services, &path).unwrap_err();
        let mut record = SessionRecord::new(root.path().to_owned());
        record.account = Some(account.into());
        record.target_selection = document.binding.target_selection;
        let resumed = crate::build_features::admit_session(&record).unwrap_err();
        for failure in [imported, resumed] {
            assert!(matches!(
                std::error::Error::source(&failure).unwrap().downcast_ref::<AccountAdmissionError>(),
                Some(AccountAdmissionError::AccountTargetMismatch { account: refused }) if refused == account
            ));
        }
        assert_eq!(services.product_store_opens(), 0);
    }
}

#[test]
fn another_version_or_format_is_refused_by_name() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("doc.json");
    let mut value = serde_json::to_value(sample_document()).unwrap();
    value["version"] = serde_json::json!(7);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let error = read_document(&path).unwrap_err();
    assert!(error.to_string().contains("version 7"), "{error}");
    value["format"] = serde_json::json!("something-else");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        read_document(&path)
            .unwrap_err()
            .to_string()
            .contains("not a session export")
    );
}

/// The session header is derived from the record and never stored (spec
/// 02 §5.4): a document that plants one naming another session and
/// source must not render it under the real header.
#[test]
fn an_import_never_stores_a_session_header_row() {
    let mut document = sample_document();
    document.transcript.push(TranscriptRow::SessionHeader {
        session_id: "00000000-0000-4000-8000-000000000000".into(),
        carried_from: None,
    });
    let CarriedImport { record, .. } = document.into_import().unwrap();
    assert!(
        !record
            .transcript
            .iter()
            .any(|row| matches!(row, TranscriptRow::SessionHeader { .. })),
        "a stored header row would render as a second session header"
    );
}

/// Imported rows take the path live rows take: a truncation marker stays
/// only as the first row, and tool and thinking rows are bounded exactly
/// as the host bounds them before it persists them.
#[test]
fn an_import_bounds_rows_the_way_live_rows_are_bounded() {
    let mut document = sample_document();
    document.transcript = vec![
        TranscriptRow::Truncated,
        TranscriptRow::user("kept"),
        TranscriptRow::Truncated,
        TranscriptRow::Tool(ToolItem {
            wire_operation: None,
            call_id: "call-big".into(),
            name: "exec_command".into(),
            exposed_name: None,
            kind: plakous_host_protocol::views::presentation::ActivityKind::Process,
            state: "succeeded".into(),
            subject: Some(format!("first line\n{}", "s".repeat(1_000))),
            detail: Some("d".repeat(10 * 1024)),
            started_at_ms: None,
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        }),
        TranscriptRow::Thinking(ThinkingRow {
            location_key: "0:0".into(),
            text: "t".repeat(100 * 1024),
            elapsed_ms: None,
        }),
        TranscriptRow::Thinking(ThinkingRow {
            location_key: "0:1".into(),
            text: "  ".into(),
            elapsed_ms: None,
        }),
        TranscriptRow::Process(plakous_host_protocol::views::transcript::ProcessNotice {
            job_id: "p1".into(),
            outcome: plakous_host_protocol::views::transcript::ProcessOutcome::Stopped,
            duration_ms: None,
            subject: Some(format!("first line\n{}", "s".repeat(1_000))),
        }),
    ];
    let CarriedImport { record, .. } = document.into_import().unwrap();
    let rows = record.transcript.to_vec();
    assert_eq!(rows.len(), 5, "{rows:?}");
    let TranscriptRow::Process(process) = &rows[4] else {
        panic!("a process row: {:?}", rows[4]);
    };
    let subject = process.subject.as_deref().unwrap();
    assert!(subject.len() <= 240 && !subject.contains('\n'), "{subject}");
    assert_eq!(rows[0], TranscriptRow::Truncated);
    assert_eq!(rows[1], TranscriptRow::user("kept"));
    let TranscriptRow::Tool(tool) = &rows[2] else {
        panic!("a tool row: {:?}", rows[2]);
    };
    let subject = tool.subject.as_deref().unwrap();
    assert!(subject.len() <= 240 && !subject.contains('\n'), "{subject}");
    assert!(
        tool.detail.as_deref().unwrap().len()
            <= plakous_hosted::sessions::MAX_TOOL_ACTIVITY_DETAIL_BYTES
    );
    let TranscriptRow::Thinking(thinking) = &rows[3] else {
        panic!("a thinking row: {:?}", rows[3]);
    };
    assert!(thinking.text.len() <= plakous_hosted::sessions::MAX_THINKING_ACTIVITY_BYTES);
}

/// A document whose transcript holds more rows than any session can is
/// refused while it is read.
#[test]
fn a_document_past_the_transcript_bound_is_refused() {
    let mut value = serde_json::to_value(sample_document()).unwrap();
    value["transcript"] = serde_json::Value::Array(
        (0..=MAX_TRANSCRIPT_LINES)
            .map(|_| serde_json::to_value(TranscriptRow::user("x")).unwrap())
            .collect(),
    );
    let error = serde_json::from_str::<CarryDocument>(&value.to_string()).unwrap_err();
    assert!(error.to_string().contains("more than"), "{error}");
    value["transcript"] = serde_json::Value::Array(
        (0..20)
            .map(|_| serde_json::to_value(TranscriptRow::user("x".repeat(1024 * 1024))).unwrap())
            .collect(),
    );
    let error = serde_json::from_str::<CarryDocument>(&value.to_string()).unwrap_err();
    assert!(error.to_string().contains("exceeds"), "{error}");
}

#[test]
fn an_import_links_back_and_holds_its_queue() {
    let CarriedImport {
        record,
        history,
        note,
        ..
    } = sample_document().into_import().unwrap();
    assert_eq!(
        note,
        format!(
            "session {} linked from 5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11 (r1.e1.p1.t1, producer binary abcdef012345); conversation carried; 0 messages, ~0 tokens; the first request rebuilds the prompt cache",
            record.id
        )
    );
    assert_ne!(record.id, "5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11");
    assert_eq!(
        record.carried_forward_from(),
        Some(("5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11", "r1.e1.p1.t1"))
    );
    assert_eq!(
        carried_forward_label(&record).as_deref(),
        Some("carried forward from 5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11 (r1.e1.p1.t1)")
    );
    record.validate_for_store().unwrap();
    assert_eq!(record.title.as_deref(), Some("carried"));
    assert_eq!(record.model.as_deref(), Some("offline-loopback-v1"));
    assert_eq!(
        record
            .pending_queue
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        ["held follow-up"]
    );
    assert_eq!(record.queue_drain_state, QueueDrainState::RestoredPaused);
    assert!(record.plakous_locator.is_none());
    assert_eq!(history.replay_account(), "");
}

#[test]
fn export_refuses_a_session_without_a_runtime() {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    let record = SessionRecord::new(root.path().to_path_buf());
    assert_eq!(
        export_session(&paths, &record).unwrap_err(),
        ExportRefusal::NoRuntime
    );
}

#[test]
fn carry_capacity_warning_names_the_route_window_and_first_prompt_refusal() {
    let mut capacity = CarryCapacityProbe {
        route: "destination.route".into(),
        total_window: Some(1000),
        admitted_input: Some(800),
        estimated_tokens: 900,
        messages: 10,
    };
    let warning = capacity.warning().unwrap();
    assert!(
        warning.contains("destination.route input window 800 tokens (total window 1000 tokens)")
    );
    assert!(warning.contains("first prompt will refuse unless the history compacts"));
    capacity.estimated_tokens = 800;
    assert_eq!(capacity.warning(), None);
}

#[test]
fn oversized_export_document_refuses_before_parsing() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("too-large.json");
    fs::File::create(&path)
        .unwrap()
        .set_len(256 * 1024 * 1024 + 1)
        .unwrap();
    assert!(matches!(
        read_document(&path),
        Err(ImportError::Oversized {
            section: "document",
            ..
        })
    ));
}

fn document(version: u32, transcript: serde_json::Value) -> CarryDocument {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.json");
    let mut value = serde_json::to_value(sample_document()).unwrap();
    value["version"] = serde_json::json!(version);
    value["transcript"] = transcript;
    fs::write(&path, value.to_string()).unwrap();
    read_document(&path).unwrap()
}

/// A process command reaches the stored transcript through the subject
/// redactor live (spec 04 §5); an imported row is only re-bounded, so a
/// credential in a carried command is stored and shown as written.
#[test]
fn an_imported_process_command_is_redacted_like_a_live_one() {
    let document = document(
        VERSION,
        serde_json::json!([{
            "kind": "process",
            "job_id": "p1",
            "outcome": "stopped",
            "subject": "GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list"
        }]),
    );
    let CarriedImport { record, .. } = document.into_import().unwrap();
    let subject = record
        .transcript
        .iter()
        .cloned()
        .find_map(|row| match row {
            TranscriptRow::Process(process) => process.subject,
            _ => None,
        })
        .unwrap();
    assert!(!subject.contains("ghp_"), "{subject}");
}

/// A tool row's detail is an argument or result preview, redacted on the
/// live path (spec 04 §5); an imported row's detail is redacted too, so a
/// carried credential is never stored or shown when the row is opened.
#[test]
fn an_imported_tool_detail_is_redacted_like_a_live_one() {
    let tool = crate::store::every_stored_row_type()
        .into_iter()
        .find(|row| matches!(row, TranscriptRow::Tool(_)))
        .unwrap();
    let mut row = serde_json::to_value(tool).unwrap();
    row["detail"] = serde_json::json!("GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list");
    let document = document(VERSION, serde_json::json!([row]));
    let CarriedImport { record, .. } = document.into_import().unwrap();
    let detail = record
        .transcript
        .iter()
        .cloned()
        .find_map(|row| match row {
            TranscriptRow::Tool(tool) => tool.detail,
            _ => None,
        })
        .unwrap();
    assert!(!detail.contains("ghp_"), "{detail}");
}
