#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::projector::{
    Delivery, ProductIntents, SessionViews, Submission, SubmissionKind, ViewWrites,
};
use crate::session_store::{MemoryStore, SessionRecordStore};

#[test]
fn skipped_stored_rows_do_not_leak_keys_and_pending_notice_identity_survives_reload() {
    use super::projector::{Lane, RowKey};
    let retained = vec![(
        RowKey::new(5, Lane::Runtime, 0),
        TranscriptRow::user("kept"),
    )];
    let mut input = retained.clone();
    input.push((
        RowKey::new(9, Lane::Runtime, 0),
        TranscriptRow::SessionHeader {
            session_id: "skipped".into(),
            carried_from: None,
        },
    ));
    let mut transcript = ProductTranscript::stored(input);
    transcript.record_notice(TranscriptRow::info(NoticeScope::Tree, "pending"));
    let pending = transcript.pending_notice_items();
    let key = pending[0].0.key;
    assert_ne!(key, RowKey::new(9, Lane::Runtime, 0));
    assert_eq!(transcript.items().last().unwrap().key, key);
    let mut reloaded = ProductTranscript::stored(retained);
    for (item, id) in pending {
        reloaded.record_notice_item(item, id);
    }
    assert_eq!(transcript.items(), reloaded.items());
}

#[test]
fn stored_transcript_keeps_projector_keys_through_tool_updates_and_live_notices() {
    use super::projector::{Lane, RowKey};
    let mut tool = ToolItem {
        call_id: "call".into(),
        name: "read_file".into(),
        exposed_name: None,
        kind: plakous_host_protocol::views::ActivityKind::Tool,
        state: "running".into(),
        subject: None,
        detail: None,
        started_at_ms: None,
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: None,
        wait: None,
    };
    let keys = [
        RowKey::new(7, Lane::Runtime, 2),
        RowKey::new(13, Lane::Runtime, 0),
    ];
    let mut transcript = ProductTranscript::stored(vec![
        (RowKey::TRUNCATION, TranscriptRow::Truncated),
        (keys[0], TranscriptRow::user("prompt")),
        (keys[1], TranscriptRow::Tool(tool.clone())),
    ]);
    tool.state = "succeeded".into();
    transcript.upsert_tool(&tool);
    transcript.record_notice(TranscriptRow::info(NoticeScope::Tree, "notice"));
    let items = transcript.items();
    assert_eq!(items[0].key, RowKey::new(0, Lane::Runtime, 1));
    assert_eq!(items[1].key, keys[0]);
    assert_eq!(items[2].key, keys[1]);
    assert_eq!(items[2].row, TranscriptRow::Tool(tool));
    assert!(items[3].key > keys[1]);
    assert!(items.windows(2).all(|items| items[0].key < items[1].key));
}

fn child_result(subject: &str) -> TranscriptRow {
    TranscriptRow::Tool(ToolItem {
        call_id: "child-result:m1".into(),
        name: "child_result".into(),
        exposed_name: None,
        kind: crate::activity::tool_activity_kind("child_result"),
        state: "done".into(),
        subject: Some(subject.into()),
        detail: None,
        started_at_ms: None,
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: None,
        wait: None,
    })
}

#[test]
fn rows_sharing_a_call_id_keep_their_own_keys_stored_and_live() {
    use super::projector::{Lane, RowKey};
    // A carried session's imported result, then its own tree's first result:
    // the successor tree numbers its mail from the start again.
    let rows = vec![
        (
            RowKey::new(0, Lane::Product, 0),
            TranscriptRow::user("before the carry"),
        ),
        (
            RowKey::new(0, Lane::Product, 1),
            child_result("m1 · c1 completed"),
        ),
        (
            RowKey::new(0, Lane::Product, 2),
            TranscriptRow::assistant("carried answer"),
        ),
        (
            RowKey::new(5, Lane::Runtime, 0),
            TranscriptRow::user("after the carry"),
        ),
        (
            RowKey::new(6, Lane::Product, 0),
            child_result("m1 · c2 completed"),
        ),
    ];
    let stored: Vec<_> = ProductTranscript::stored(rows.clone())
        .items()
        .into_iter()
        .map(|item| (item.key, item.row))
        .collect();
    assert_eq!(stored, rows);

    let mut live = ProductTranscript::stored(rows[..4].to_vec());
    live.record_named_notice(
        "tree:successor:result:m1".into(),
        child_result("m1 · c2 completed"),
    );
    let items = live.items();
    assert!(items.windows(2).all(|items| items[0].key < items[1].key));
    assert_eq!(items[1].row, child_result("m1 · c1 completed"));
    assert_eq!(items[4].row, child_result("m1 · c2 completed"));
    assert_eq!(
        live.tool_activity("child-result:m1")
            .unwrap()
            .subject
            .as_deref(),
        Some("m1 · c2 completed"),
        "the newest row owns the call id"
    );
}

#[test]
fn a_document_with_rows_sharing_a_call_id_keeps_every_row() {
    let lines = vec![
        TranscriptRow::user("before the carry"),
        child_result("m1 · c1 completed"),
        TranscriptRow::assistant("carried answer"),
        child_result("m1 · c2 completed"),
    ];
    let transcript = ProductTranscript::from_lines(lines.clone());
    assert_eq!(transcript.to_vec(), lines);
    assert!(
        transcript
            .items()
            .windows(2)
            .all(|items| items[0].key < items[1].key)
    );
    assert_eq!(
        ProductTranscript::from_lines(transcript.to_vec()).to_vec(),
        lines
    );
}

#[test]
fn a_carried_first_commit_writes_its_earlier_notices_as_notices() {
    let root = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    let mut record = SessionRecord::new(root.path().to_owned());
    record.link_carried_forward("00000000-0000-4000-8000-0000000000aa", "carried-state");
    let imported = vec![
        TranscriptRow::user("imported prompt"),
        TranscriptRow::assistant("imported answer"),
    ];
    record.transcript = ProductTranscript::from_lines(imported.clone());
    let named = TranscriptRow::info(NoticeScope::Tree, "named before the first commit");
    record
        .transcript
        .record_named_notice("tree:carried:milestone:1".into(), named.clone());
    let unnamed = TranscriptRow::info(NoticeScope::Session, "carried forward");
    record.record_notice(unnamed.clone());
    assert_eq!(
        record.transcript.imported_rows(),
        imported.iter().collect::<Vec<_>>()
    );
    store.save(&mut record).unwrap();

    // Imported rows and notices are one key space: the notices follow the
    // imported rows at anchor 0, under their own identities.
    let intents = store.load_intents(&record.id).unwrap();
    let notices: Vec<_> = intents
        .notices
        .iter()
        .map(|notice| {
            (
                notice
                    .notice_id
                    .starts_with(projector::IMPORTED_NOTICE_PREFIX),
                notice.key(),
                notice.row.clone(),
            )
        })
        .collect();
    let key = |ordinal| projector::RowKey::new(0, projector::Lane::Product, ordinal);
    assert_eq!(
        notices,
        [
            (true, key(0), imported[0].clone()),
            (true, key(1), imported[1].clone()),
            (false, key(2), named.clone()),
            (false, key(3), unnamed.clone())
        ]
    );
    assert_eq!(
        intents.notices[2].notice_id, "tree:carried:milestone:1",
        "the notice keeps its identity"
    );
    record = store.load(&record.id).unwrap();
    assert_eq!(
        record.transcript.to_vec(),
        [
            imported[0].clone(),
            imported[1].clone(),
            named.clone(),
            unnamed
        ]
    );
    // A producer's retry of the named notice is deduplicated by identity.
    record
        .transcript
        .record_named_notice("tree:carried:milestone:1".into(), named);
    store.save(&mut record).unwrap();
    assert_eq!(store.load_intents(&record.id).unwrap().notices.len(), 4);
}

#[test]
fn a_rebuilt_projection_restores_pending_notices_once_in_key_order() {
    let root = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    let mut record = SessionRecord::new(root.path().to_owned());
    let committed = TranscriptRow::info(NoticeScope::Tree, "durable start");
    record
        .transcript
        .record_named_notice("attempt:start".into(), committed.clone());
    store.save(&mut record).unwrap();
    record = store.load(&record.id).unwrap();
    // A producer's retry of the committed notice, then two unsaved ones.
    record
        .transcript
        .record_named_notice("attempt:start".into(), committed.clone());
    let unsaved = [
        TranscriptRow::info(NoticeScope::Tree, "first unsaved"),
        TranscriptRow::info(NoticeScope::Tree, "second unsaved"),
    ];
    for row in &unsaved {
        record.transcript.record_notice(row.clone());
    }
    let intents = store.load_intents(&record.id).unwrap();
    let views = store
        .load_views(&record.id)
        .unwrap()
        .unwrap_or_else(SessionViews::empty);
    let mut rebuilt = ProductTranscript::stored(views.transcript(&intents));
    let mut pending = record.transcript.pending_notice_items();
    pending.reverse();
    rebuilt.restore_pending_notices(pending, &intents);
    let items = rebuilt.items();
    assert!(items.windows(2).all(|items| items[0].key < items[1].key));
    let rows: Vec<_> = items.into_iter().map(|item| item.row).collect();
    assert_eq!(rows, [committed, unsaved[0].clone(), unsaved[1].clone()]);
    assert_eq!(rebuilt.pending_notices(), unsaved);
}

fn submission(item: &QueueItem, command: &str) -> Submission {
    Submission {
        command_id: command.into(),
        kind: SubmissionKind::RootStart,
        queue_item_id: item.id.clone(),
        queue_item_revision: item.revision,
        target_turn_id: None,
        canonical_command: item.text.clone(),
        submitted_at_ms: item.submitted_at_ms,
    }
}

fn project_delivery(
    store: &MemoryStore,
    session: &SessionRecord,
    command: &str,
    delivery: Delivery,
) {
    let mut views = SessionViews::empty();
    views.state.deliveries.insert(command.into(), delivery);
    store
        .commit_projection_views(
            &session.id,
            &ViewWrites::between(&SessionViews::empty(), &views),
            &views.state,
        )
        .unwrap();
}

use super::*;

#[test]
fn named_notice_retries_are_idempotent_and_identity_collisions_are_atomic() {
    use crate::session_store::{MemoryStore, SessionRecordStore};
    let root = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    let mut record = SessionRecord::new(root.path().to_owned());
    let row = TranscriptRow::info(NoticeScope::Tree, "durable start");
    record
        .transcript
        .record_named_notice("attempt:start".into(), row.clone());
    store.save(&mut record).unwrap();
    record = store.load(&record.id).unwrap();
    record
        .transcript
        .record_named_notice("attempt:start".into(), row);
    store.save(&mut record).unwrap();
    assert_eq!(store.load_intents(&record.id).unwrap().notices.len(), 1);
    record.transcript.record_named_notice(
        "new-but-uncommitted".into(),
        TranscriptRow::info(NoticeScope::Tree, "not committed"),
    );
    record.transcript.record_named_notice(
        "attempt:start".into(),
        TranscriptRow::info(NoticeScope::Tree, "different identity payload"),
    );
    assert!(store.save(&mut record).is_err());
    assert_eq!(store.load_intents(&record.id).unwrap().notices.len(), 1);
}
use plakous_host_protocol::views::presentation::row_bytes;

#[test]
fn the_root_turn_count_advances_per_admitted_turn() {
    let mut session = SessionRecord::new(PathBuf::from("/work/exact"));
    assert_eq!(session.root_turn_count, 0);
    for expected in 1..=10 {
        session.record_admitted_root_turn();
        assert_eq!(session.root_turn_count, expected);
    }
    assert!(!session.queue_commands.has_pending());
}

fn running_activity(call_id: &str) -> ToolItem {
    ToolItem {
        parent_call_id: None,
        cell_id: None,
        call_id: call_id.to_owned(),
        name: "read".to_owned(),
        exposed_name: None,
        kind: crate::activity::tool_activity_kind("read"),
        state: "running".to_owned(),
        subject: Some(format!("subject {call_id}")),
        detail: None,
        started_at_ms: Some(1),
        finished_at_ms: None,
        wait: None,
    }
}

#[test]
fn written_subject_does_not_show_a_preview_marker() {
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    let mut activity = running_activity("truncated");
    activity.subject = Some("first\n[preview truncated]".into());
    session.upsert_tool_activity(activity);
    assert_eq!(
        session
            .transcript
            .tool_activity("truncated")
            .unwrap()
            .subject
            .as_deref(),
        Some("first…")
    );
}

#[test]
fn written_subject_is_single_line_and_byte_bounded() {
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    let mut activity = running_activity("a");
    activity.subject = Some(format!("first\n{}", "界".repeat(200)));
    session.upsert_tool_activity(activity);
    let subject = session
        .transcript
        .tool_activity("a")
        .unwrap()
        .subject
        .unwrap();
    assert!(subject.len() <= 240);
    assert!(subject.contains('↵'));
    assert!(subject.ends_with('…'));
    assert!(!subject.contains('\n'));
}

fn tool_row(subject: String, detail: String) -> TranscriptRow {
    TranscriptRow::Tool(ToolItem {
        subject: Some(subject),
        detail: Some(detail),
        ..running_activity("bounded")
    })
}

/// A cut that ends exactly twenty characters into `AKIA` and seventeen
/// characters completes `\bAKIA[0-9A-Z]{16}\b` at the marker; a cut right
/// after a redacted header name leaves the ellipsis as its value. The stored
/// row is redacted and a fixed point of `bound_row` all the same.
#[test]
fn a_cut_never_completes_a_credential_shape() {
    let key = "AKIAABCDEFGHIJKLMNOPQ";
    // The subject keeps 237 bytes before its ellipsis, the detail 2,028
    // before its marker: both cuts land right after `AKIAABCDEFGHIJKLMNOP`.
    let subject = format!("{} {key} tail", "x".repeat(216));
    let detail = format!("{} {key} tail {}", "y".repeat(2_007), "z".repeat(40));
    for (subject, detail) in [
        (subject, detail),
        (
            format!("{} Cookie: abc tail", "x".repeat(228)),
            format!("{}\nCookie: abc\n{}", "y".repeat(2_019), "z".repeat(100)),
        ),
    ] {
        let once = bound_row(tool_row(subject, detail));
        assert_eq!(bound_row(once.clone()), once);
        let tool = once.as_tool().unwrap();
        for text in [tool.subject.as_deref(), tool.detail.as_deref()] {
            let text = text.unwrap();
            assert_eq!(plakous::tools::preview_redaction::text(text), text);
            assert!(!text.contains("AKIAABCDEFGHIJKLMNOP"), "{text}");
        }
        assert!(tool.subject.as_deref().unwrap().len() <= 240);
        assert!(tool.detail.as_deref().unwrap().len() <= MAX_TOOL_ACTIVITY_DETAIL_BYTES);
    }
}

fn credential() -> impl proptest::strategy::Strategy<Value = String> {
    use proptest::prelude::*;
    prop_oneof![
        "[0-9A-Z]{14,20}".prop_map(|body| format!("AKIA{body}")),
        "[A-Za-z0-9_-]{18,26}".prop_map(|body| format!("sk-{body}")),
        "[A-Za-z0-9._~+/-]{14,20}".prop_map(|body| format!("Authorization: Bearer {body}")),
        "[A-Za-z0-9._~+/-]{14,20}".prop_map(|body| format!("Bearer {body}")),
        "[A-Za-z0-9]{6,12}".prop_map(|body| format!("api_key={body}")),
        "[A-Za-z0-9]{6,12}".prop_map(|body| format!("\"password\": \"{body}\"")),
        "[A-Za-z0-9]{1,4}".prop_map(|body| format!("Cookie: {body}")),
        "[a-z0-9]{1,10}".prop_map(|body| format!("https://h/p?token={body}")),
    ]
}

/// Text whose credentials sit within a few dozen bytes of `bound`.
fn near(bound: usize) -> impl proptest::strategy::Strategy<Value = String> {
    use proptest::prelude::*;
    (
        0_usize..72,
        prop::collection::vec(
            (
                prop_oneof![Just(" "), Just("\n"), Just(":"), Just("="), Just("-")],
                credential(),
            ),
            1..4,
        ),
        "[ a-zA-Z0-9]{0,24}",
    )
        .prop_map(move |(offset, parts, tail)| {
            let mut text = "a".repeat(bound.saturating_sub(offset));
            for (separator, credential) in parts {
                text.push_str(separator);
                text.push_str(&credential);
            }
            text.push_str(&tail);
            text
        })
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

    #[test]
    fn bound_rows_are_fixed_points(
        subject in near(240),
        detail in near(MAX_TOOL_ACTIVITY_DETAIL_BYTES),
        thinking in near(MAX_THINKING_ACTIVITY_BYTES),
    ) {
        let tool = bound_row(tool_row(subject, detail));
        proptest::prop_assert_eq!(bound_row(tool.clone()), tool);
        let thinking = bound_row(TranscriptRow::Thinking(ThinkingRow {
            location_key: "0:0".to_owned(),
            text: thinking,
            elapsed_ms: None,
        }));
        proptest::prop_assert_eq!(bound_row(thinking.clone()), thinking);
    }
}

/// A stored projector row an older bound wrote is shown as this build
/// bounds it: a process row's heredoc token, kept by the subject bound
/// alone, is redacted on read. A row this build bounded is shown as stored.
#[test]
fn stored_rows_are_bounded_again_on_read() {
    use super::projector::{Lane, RowKey};
    let stale = TranscriptRow::Process(plakous_host_protocol::views::transcript::ProcessNotice {
        job_id: "p1".to_owned(),
        outcome: plakous_host_protocol::views::transcript::ProcessOutcome::Exited { code: Some(0) },
        duration_ms: Some(5),
        subject: Some(
            "gh auth login --with-token <<EOF\\nghp_abcdefghijklmnopqrstuvwxyz0123\\nEOF"
                .to_owned(),
        ),
    });
    let current = bound_row(TranscriptRow::user("kept as stored"));
    let transcript = ProductTranscript::stored(vec![
        (RowKey::new(3, Lane::Runtime, 0), stale.clone()),
        (RowKey::new(4, Lane::Runtime, 0), current.clone()),
    ]);
    let shown: Vec<TranscriptRow> = transcript
        .items()
        .iter()
        .map(|item| item.row.clone())
        .collect();
    assert_eq!(shown, vec![bound_row(stale), current]);
    let TranscriptRow::Process(process) = &shown[0] else {
        panic!("the process row keeps its kind");
    };
    let subject = process.subject.as_deref().unwrap();
    assert!(!subject.contains("ghp_"), "{subject}");
    assert!(
        subject.starts_with("gh auth login --with-token <<EOF\\n"),
        "{subject}"
    );
}

#[test]
fn blank_thinking_is_not_a_row() {
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    session.append_thinking_activity(ThinkingRow {
        location_key: "0:0".into(),
        text: "  ".into(),
        elapsed_ms: None,
    });
    assert!(session.transcript.is_empty());
    session.append_thinking_activity(ThinkingRow {
        location_key: "0:0".into(),
        text: "inventory the repo".into(),
        elapsed_ms: Some(4100),
    });
    assert!(matches!(
        &session.transcript[..],
        [TranscriptRow::Thinking(row)] if row.elapsed_ms == Some(4100)
    ));
}

#[test]
fn a_tool_row_update_is_a_replace_addressed_by_call_id() {
    let mut transcript = ProductTranscript::default();
    for index in 0..1_000 {
        assert!(transcript.upsert_tool(&running_activity(&format!("call-{index}"))));
    }
    transcript.mark_committed();

    for index in 0..200 {
        let mut done = running_activity(&format!("call-{index}"));
        "succeeded".clone_into(&mut done.state);
        assert!(
            !transcript.upsert_tool(&done),
            "a known call id is a replace"
        );
    }
    assert_eq!(transcript.len(), 1_000);

    assert_eq!(
        transcript.tool_activity("call-150").map(|row| row.state),
        Some("succeeded".to_owned())
    );
    assert!(transcript.tool_activity("call-absent").is_none());
}

#[test]
fn a_bounded_transcript_drops_in_batches_and_keeps_call_rows_addressable() {
    let mut transcript = ProductTranscript::default();
    for index in 0..MAX_TRANSCRIPT_LINES {
        transcript.upsert_tool(&running_activity(&format!("call-{index}")));
    }
    transcript.mark_committed();
    assert_eq!(transcript.len(), MAX_TRANSCRIPT_LINES);

    transcript.append(TranscriptRow::assistant("newest"));
    let kept = MAX_TRANSCRIPT_LINES - MAX_TRANSCRIPT_LINES / 8;
    assert_eq!(transcript.len(), kept);
    assert_eq!(transcript[0], TranscriptRow::Truncated);
    assert_eq!(transcript.last(), Some(&TranscriptRow::assistant("newest")));
    let dropped = MAX_TRANSCRIPT_LINES + 1 - (kept - 1);
    assert!(transcript.tool_activity("call-0").is_none());
    let survivor = format!("call-{dropped}");
    let mut done = running_activity(&survivor);
    "succeeded".clone_into(&mut done.state);
    assert!(!transcript.upsert_tool(&done));
    assert_eq!(
        transcript.tool_activity(&survivor).map(|row| row.state),
        Some("succeeded".to_owned())
    );
    assert_eq!(transcript[1], TranscriptRow::Tool(done));

    // The next append under the bound drops nothing: batches, not churn.
    transcript.mark_committed();
    transcript.append(TranscriptRow::assistant("after"));
    assert_eq!(transcript.len(), kept + 1);
}

#[test]
fn stored_rows_rebuild_the_same_projection_and_identities() {
    let mut live = ProductTranscript::default();
    live.append(TranscriptRow::user("go"));
    live.upsert_tool(&running_activity("call-a"));
    live.append(TranscriptRow::assistant("done"));
    let rows = live
        .rows()
        .into_iter()
        .map(|(row, call_id)| (row.clone(), call_id.map(str::to_owned)))
        .collect::<Vec<_>>();
    assert_eq!(rows[1].1.as_deref(), Some("call-a"));
    let restored = ProductTranscript::from_rows(rows);
    assert_eq!(restored, live);
    assert_eq!(restored.pending_notices(), []);
    assert_eq!(
        restored.tool_activity("call-a").map(|row| row.call_id),
        Some("call-a".to_owned())
    );
}

fn locator(checkpoint: u8) -> SessionLocator {
    format!(
        "00000000-0000-0000-0000-000000000001/\
             00000000-0000-0000-0000-000000000002/\
             00000000-0000-0000-0000-{checkpoint:012}"
    )
    .parse()
    .unwrap()
}

fn branch_locator(branch: u8, checkpoint: u8) -> SessionLocator {
    format!(
        "00000000-0000-0000-0000-000000000001/\
             00000000-0000-0000-0000-{branch:012}/\
             00000000-0000-0000-0000-{checkpoint:012}"
    )
    .parse()
    .unwrap()
}

fn rebind_selection(model: &str) -> Box<RebindSelection> {
    Box::new(RebindSelection {
        provider: "xai".into(),
        account: "grok".into(),
        model: model.into(),
        effort: Some("high".into()),
        composition: ProductCompositionSelection::FollowCatalog,
        target_selection: ProductTargetSelection::Native,
        protocol: "responses".into(),
        route: "xai:oauth:responses".into(),
        requested_model: model.into(),
        selected_configuration: "selected-config".into(),
        effective_configuration: "effective-config".into(),
        account_binding: "account-binding".into(),
        own_tree: Some(RootProfile::default()),
    })
}

fn openrouter_target() -> ProductTargetSelection {
    use crate::binding::{OpenRouterRoutingSelection, ProtocolSelection, WireBinding};
    ProductTargetSelection::OpenRouter {
        wire: WireBinding::resolve(ProtocolSelection::Auto),
        routing: OpenRouterRoutingSelection::GatewayManaged,
    }
}

fn bound_session(workspace_root: impl Into<PathBuf>) -> SessionRecord {
    let mut session = SessionRecord::new(workspace_root.into());
    session.bind_plakous_locator(locator(3)).unwrap();
    session
}

#[test]
fn product_session_identity_has_one_canonical_spelling() {
    let exact = "018f2537-5ed2-7b11-a4b1-f009be85f101";
    assert_eq!(canonical_product_session_id(exact), Ok(exact));
    for invalid in [
        "018F2537-5ED2-7B11-A4B1-F009BE85F101",
        "018f25375ed27b11a4b1f009be85f101",
        "../018f2537-5ed2-7b11-a4b1-f009be85f101",
    ] {
        assert!(canonical_product_session_id(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn prompt_history_suppresses_adjacent_duplicates_and_enforces_both_bounds() {
    let mut session = bound_session("/work/prompt-history-bounds");
    session.record_prompt("same".into());
    session.record_prompt("same".into());
    assert_eq!(session.prompt_history, ["same"]);

    for index in 0..=MAX_PROMPT_HISTORY {
        session.record_prompt(format!("prompt-{index}"));
    }
    assert_eq!(session.prompt_history.len(), MAX_PROMPT_HISTORY);
    assert_eq!(
        session.prompt_history.first().map(String::as_str),
        Some("prompt-1")
    );

    let large = "x".repeat(MAX_PROMPT_BYTES - 1);
    for index in 0..5 {
        session.record_prompt(format!("{index}{large}"));
    }
    assert!(
        session
            .prompt_history
            .iter()
            .map(String::len)
            .sum::<usize>()
            <= MAX_PROMPT_HISTORY_BYTES
    );
    assert_eq!(session.prompt_history.len(), 4);
    assert!(session.validate_for_store().is_ok());
}

#[test]
fn queue_bounds_are_enforced_by_typed_store_validation() {
    let mut session = bound_session("/work/queue-bounds");
    session.pending_queue.push(QueueItem::new("ok"));
    let oversized = "x".repeat(MAX_PROMPT_BYTES + 1);
    session.pending_queue[0].text = oversized;
    assert!(session.validate_for_store().is_err());
    session.pending_queue = (0..=MAX_PENDING_PROMPTS)
        .map(|index| QueueItem::new(format!("item-{index}")))
        .collect();
    assert!(session.validate_for_store().is_err());
}

#[test]
fn product_transcript_projection_is_bounded_without_touching_locator() {
    let mut session = bound_session("/work/long");
    let authority = session.plakous_locator;
    session.transcript = ProductTranscript::from_lines(
        (0..MAX_TRANSCRIPT_LINES + 50)
            .map(|index| TranscriptRow::assistant(format!("row {index}")))
            .collect(),
    );
    session.append_transcript(TranscriptRow::assistant("newest"));

    assert!(session.transcript.len() <= MAX_TRANSCRIPT_LINES);
    assert_eq!(session.transcript.first(), Some(&TranscriptRow::Truncated));
    assert_eq!(
        session.transcript.last(),
        Some(&TranscriptRow::assistant("newest"))
    );
    assert_eq!(session.plakous_locator, authority);

    let mut oversized = bound_session("/work/oversized");
    oversized.append_transcript(TranscriptRow::assistant(format!(
        "{}tail",
        "x".repeat(MAX_TRANSCRIPT_BYTES + 128)
    )));
    assert_eq!(oversized.transcript.len(), 1);
    let row = &oversized.transcript[0];
    assert!(row_bytes(row) <= MAX_TRANSCRIPT_BYTES);
    let TranscriptRow::Assistant { text } = row else {
        panic!("{row:?}");
    };
    assert!(text.starts_with(TRANSCRIPT_TRUNCATION_MARKER));
    assert!(text.ends_with("tail"));
}

#[test]
fn pending_rejections_survive_settled_submissions_at_full_queue_capacity() {
    let mut session = bound_session("/work/full-queue");
    let mut intents = ProductIntents::default();
    let mut views = SessionViews::empty();
    for index in 0..MAX_PENDING_PROMPTS {
        let item = QueueItem::new("rejected");
        let request = submission(&item, &format!("rejected:{index}"));
        views.state.deliveries.insert(
            request.command_id.clone(),
            Delivery::Rejected {
                error: "refused".into(),
            },
        );
        intents.submissions.push(request);
        session.pending_queue.push(item);
    }
    for index in 0..=MAX_PENDING_PROMPTS + 128 {
        let request = submission(&QueueItem::new("consumed"), &format!("consumed:{index}"));
        views
            .state
            .deliveries
            .insert(request.command_id.clone(), Delivery::Consumed);
        intents.submissions.push(request);
    }
    session.queue_commands = QueueCommands::from_join(&intents, Some(&views));
    assert_eq!(session.pending_queue.len(), MAX_PENDING_PROMPTS);
    for item in &session.pending_queue {
        assert!(session.queue_commands.is_rejected(item));
        assert!(!session.queue_commands.is_locked(&item.id));
        assert!(!session.queue_commands.is_consumed(item));
    }
    assert!(!session.queue_commands.has_pending());
    session.validate_for_store().unwrap();
}

#[test]
fn submission_is_immutable_idempotent_and_consumes_exact_item_revision() {
    let store = MemoryStore::new();
    let mut session = bound_session("/work/submission");
    let item = QueueItem::new("exact prompt");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "plak.command.one");
    store.save_submission(&mut session, &request).unwrap();
    store.save_submission(&mut session, &request).unwrap();
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request.clone()]
    );
    assert!(session.queue_commands.is_locked(&item.id));
    project_delivery(&store, &session, &request.command_id, Delivery::Accepted);
    assert_eq!(
        store.load(&session.id).unwrap().pending_queue,
        [] as [QueueItem; 0]
    );
    session.pending_queue[0].revision += 1;
    session.pending_queue[0].text = "new revision".into();
    store.save(&mut session).unwrap();
    assert_eq!(
        store.load(&session.id).unwrap().pending_queue,
        session.pending_queue
    );
    let conflict = Submission {
        canonical_command: "different".into(),
        ..request
    };
    assert!(store.save_submission(&mut session, &conflict).is_err());
}

#[test]
fn command_binding_is_independent_of_the_projected_queue() {
    let store = MemoryStore::new();
    let mut session = bound_session("/work/locked-submission");
    let item = QueueItem::new("immutable retry");
    session.pending_queue.push(item.clone());
    let request = submission(&item, "plak.command.locked");
    store.save_submission(&mut session, &request).unwrap();
    session.pending_queue.clear();
    assert!(session.validate_for_store().is_ok());
    store.save(&mut session).unwrap();
    let changed = Submission {
        canonical_command: "mutated".into(),
        ..request.clone()
    };
    assert!(store.save_submission(&mut session, &changed).is_err());
    assert_eq!(
        store.load_intents(&session.id).unwrap().submissions,
        vec![request]
    );
}

#[test]
fn rejected_submission_retains_item_and_pauses_restored_drain() {
    let store = MemoryStore::new();
    let mut session = bound_session("/work/rejected");
    let item = QueueItem::new("retain me");
    session.pending_queue.push(item.clone());
    let request = Submission {
        kind: SubmissionKind::SendNowSteer,
        target_turn_id: Some("00000000-0000-4000-8000-000000000009".into()),
        ..submission(&item, "plak.command.reject")
    };
    store.save_submission(&mut session, &request).unwrap();
    project_delivery(
        &store,
        &session,
        &request.command_id,
        Delivery::Rejected {
            error: "refused".into(),
        },
    );
    let loaded = store.load(&session.id).unwrap();
    assert_eq!(loaded.pending_queue, vec![item.clone()]);
    assert_eq!(loaded.queue_drain_state, QueueDrainState::RestoredPaused);
    assert!(loaded.queue_commands.is_rejected(&item));
    assert!(!loaded.queue_commands.is_locked(&item.id));
}

#[test]
fn send_now_binds_exact_target_turn_and_cannot_become_a_root() {
    let store = MemoryStore::new();
    let mut session = bound_session("/work/send-now");
    let item = QueueItem::new("steer exactly once");
    session.pending_queue.push(item.clone());
    let request = Submission {
        kind: SubmissionKind::SendNowSteer,
        target_turn_id: Some("00000000-0000-4000-8000-000000000042".into()),
        ..submission(&item, "plak.command.send-now.exact")
    };
    store.save_submission(&mut session, &request).unwrap();
    let loaded = store.load_intents(&session.id).unwrap();
    assert_eq!(loaded.submissions, vec![request.clone()]);
    for changed in [
        Submission {
            target_turn_id: Some("00000000-0000-4000-8000-000000000043".into()),
            ..request.clone()
        },
        Submission {
            kind: SubmissionKind::RootStart,
            target_turn_id: None,
            ..request.clone()
        },
    ] {
        assert!(store.save_submission(&mut session, &changed).is_err());
    }
}

#[test]
fn explicit_effort_binding_is_round_tripped_on_the_session_record() {
    let mut session = bound_session("/work/effort");
    assert_eq!(session.effort, None);
    session.effort = Some("medium".into());
    assert_eq!(session.effort.as_deref(), Some("medium"));
    assert!(session.validate_for_store().is_ok());
}

fn successor_from_fact(
    source: &SessionRecord,
    operation: &str,
    locator: SessionLocator,
    kind: SessionWorkflowKind,
    target: SessionLocator,
) -> SessionRecord {
    let request = projector::WorkflowRequest {
        operation_id: operation.to_owned(),
        source_locator: locator,
        kind,
    };
    let target = plakous::sessions::feed::FeedLocator {
        session_id: target.session_id(),
        branch_id: target.branch_id(),
        checkpoint_id: target.checkpoint_id(),
    };
    source
        .successor_from_request(
            &request,
            &projector::Successor {
                operation_id: Some(operation.to_owned()),
                transition: plakous::sessions::feed::FeedBranchTransition::Fork,
                target,
                published_seq: plakous::sessions::feed::FeedSeq::new(1),
                created_at_unix: source.updated_at_unix,
                seed: None,
            },
        )
        .unwrap()
}

#[test]
fn fork_starts_its_own_prompt_history_without_changing_the_source() {
    let mut source = bound_session("/work/fork-history");
    source.record_prompt("source-only prompt".into());
    let mut fork = successor_from_fact(
        &source,
        "plak:fork:history",
        locator(3),
        SessionWorkflowKind::Fork,
        branch_locator(8, 10),
    );
    assert_eq!(fork.prompt_history, [] as [String; 0]);
    fork.record_prompt("fork-only prompt".into());
    assert_eq!(fork.prompt_history, ["fork-only prompt"]);
    assert_eq!(source.prompt_history, ["source-only prompt"]);
}

#[test]
fn branch_record_preserves_presentation_but_not_pending_work() {
    let mut source = bound_session("/work/branch");
    source.target_selection = openrouter_target();
    source.account = Some("codex".into());
    source.model = Some("gpt-source".into());
    source.effort = Some("medium".into());
    source.title = Some("preserved title".into());
    source
        .transcript
        .append(TranscriptRow::user("preserved presentation"));
    source
        .pending_queue
        .push(QueueItem::new("must stay source-owned"));
    let intents = ProductIntents {
        submissions: vec![submission(&source.pending_queue[0], "plak.command.source")],
        ..ProductIntents::default()
    };
    source.queue_commands = QueueCommands::from_join(&intents, None);
    assert!(source.queue_commands.has_pending());
    let mut target = successor_from_fact(
        &source,
        "plak:rebind:branch",
        locator(3),
        SessionWorkflowKind::Rebind(rebind_selection("grok-target")),
        branch_locator(9, 10),
    );
    assert_ne!(target.id, source.id);
    assert_eq!(
        target.source_product_session_id.as_deref(),
        Some(source.id.as_str())
    );
    assert_eq!(
        target.source_workflow_operation_id.as_deref(),
        Some("plak:rebind:branch")
    );
    assert_eq!(target.title, source.title);
    assert!(target.transcript.is_empty());
    assert_eq!(target.account.as_deref(), Some("grok"));
    assert_eq!(target.model.as_deref(), Some("grok-target"));
    assert_eq!(target.effort.as_deref(), Some("high"));
    assert_eq!(target.target_selection, ProductTargetSelection::Native);
    assert_eq!(target.plakous_locator, Some(branch_locator(9, 10)));
    assert_eq!(target.pending_queue, [] as [QueueItem; 0]);
    assert!(!target.queue_commands.has_pending());

    assert!(target.bind_plakous_locator(locator(11)).is_err());
    assert!(target.validate_for_store().is_ok());
}

#[test]
fn non_reasoning_rebind_preserves_absent_effort_in_target_record() {
    let mut source = bound_session("/work/non-reasoning-rebind");
    source.account = Some("codex".into());
    source.model = Some("reasoning-source".into());
    source.effort = Some("medium".into());
    let operation = "plak:rebind:non-reasoning";
    let mut selection = rebind_selection("claude-sonnet-5");
    selection.provider = "claude".into();
    selection.account = "claude".into();
    selection.effort = None;
    let target = successor_from_fact(
        &source,
        operation,
        locator(3),
        SessionWorkflowKind::Rebind(selection),
        branch_locator(8, 10),
    );
    assert_eq!(target.account.as_deref(), Some("claude"));
    assert_eq!(target.model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(target.effort, None);
    assert!(target.validate_for_store().is_ok());
}

#[test]
fn rebind_lineage_transplants_history_and_always_preserves_its_immediate_source() {
    let mut codex = bound_session("/work/provider-lineage");
    codex.account = Some("codex".into());
    codex.model = Some("gpt-5.6-sol".into());
    codex.effort = Some("medium".into());
    codex.append_transcript(TranscriptRow::assistant("I am an older Codex target"));
    codex.append_transcript(TranscriptRow::user("keep this exact history"));
    codex.record_prompt("keep this exact history".into());
    let source_projection = codex.transcript.clone();
    let source_prompts = codex.prompt_history.clone();

    let grok_operation = "plak:rebind:cross-provider";
    let mut grok_selection = rebind_selection("grok-code-fast-1");
    grok_selection.composition = ProductCompositionSelection::JsPtc;
    let grok = successor_from_fact(
        &codex,
        grok_operation,
        locator(3),
        SessionWorkflowKind::Rebind(grok_selection),
        branch_locator(12, 13),
    );

    assert_eq!(codex.account.as_deref(), Some("codex"));
    assert_eq!(codex.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(codex.effort.as_deref(), Some("medium"));
    assert_eq!(codex.transcript, source_projection);
    assert!(grok.transcript.is_empty());
    assert_eq!(grok.prompt_history, source_prompts);
    assert_eq!(
        grok.source_product_session_id.as_deref(),
        Some(codex.id.as_str())
    );
    assert_eq!(grok.account.as_deref(), Some("grok"));
    assert_eq!(grok.composition, ProductCompositionSelection::JsPtc);

    let fast_operation = "plak:rebind:effort-tier";
    let mut fast_selection = rebind_selection("gpt-5.6-sol");
    fast_selection.provider = "codex".into();
    fast_selection.account = "codex".into();
    fast_selection.effort = Some("high".into());
    fast_selection.composition = ProductCompositionSelection::FollowCatalog;
    fast_selection.target_selection = ProductTargetSelection::NativePriority;
    let fast = successor_from_fact(
        &grok,
        fast_operation,
        branch_locator(12, 13),
        SessionWorkflowKind::Rebind(fast_selection),
        branch_locator(14, 15),
    );

    assert_ne!(fast.id, codex.id, "returning to an old tuple still forks");
    assert_eq!(
        fast.source_product_session_id.as_deref(),
        Some(grok.id.as_str())
    );
    assert!(fast.transcript.is_empty());
    assert_eq!(fast.effort.as_deref(), Some("high"));
    assert_eq!(
        fast.target_selection,
        ProductTargetSelection::NativePriority
    );
    assert_eq!(grok.account.as_deref(), Some("grok"));
    assert_eq!(grok.composition, ProductCompositionSelection::JsPtc);
}

#[test]
fn fork_preserves_exact_target_selection_without_reresolution() {
    let mut source = bound_session("/work/fork-target");
    source.target_selection = openrouter_target();
    source.account = Some("openrouter".into());
    source.model = Some("vendor/model".into());
    source.effort = Some("high".into());
    source.composition = ProductCompositionSelection::Direct;
    let operation = "plak:fork:target-selection";
    let target = successor_from_fact(
        &source,
        operation,
        locator(3),
        SessionWorkflowKind::Fork,
        branch_locator(8, 9),
    );
    assert_eq!(target.target_selection, source.target_selection);
}

#[test]
fn clear_workflow_reuses_exact_create_identity_and_never_copies_source_history() {
    let mut source = bound_session("/work/clear");
    source.target_selection = openrouter_target();
    source.account = Some("openrouter".into());
    source.model = Some("gpt-target".into());
    source.effort = None;
    source.composition = ProductCompositionSelection::Direct;
    source.append_transcript(TranscriptRow::user("source-only"));
    source.record_prompt("source-only".into());
    let operation = "plak:new:exact-operation";
    let request = projector::WorkflowRequest {
        operation_id: operation.to_owned(),
        kind: SessionWorkflowKind::New,
        source_locator: locator(3),
    };
    let target = source.new_session_from_request(&request).unwrap();
    assert_eq!(target.id, request.successor_product_id());
    assert_eq!(
        target.create_operation_id,
        "plak:new:exact-operation:create"
    );
    assert_eq!(
        target.plakous_locator,
        Some(
            plakous::sessions::AgentSessions::genesis_locator_for(
                &plakous::sessions::SessionOperationId::new(&target.create_operation_id).unwrap(),
            )
            .unwrap()
        )
    );
    assert_eq!(target.target_selection, source.target_selection);
    assert!(target.transcript.is_empty());
    assert_eq!(target.prompt_history, [] as [String; 0]);
    assert_eq!(target.pending_queue, [] as [QueueItem; 0]);

    assert_eq!(
        target.source_product_session_id.as_deref(),
        Some(source.id.as_str())
    );
    assert!(target.validate_for_store().is_ok());
}

#[test]
fn locator_may_advance_only_within_the_bound_runtime_branch() {
    let mut session = bound_session("/work/exact");
    session.bind_plakous_locator(locator(4)).unwrap();
    assert_eq!(session.plakous_locator, Some(locator(4)));

    let other_branch = concat!(
        "00000000-0000-0000-0000-000000000001/",
        "00000000-0000-0000-0000-000000000099/",
        "00000000-0000-0000-0000-000000000005"
    )
    .parse()
    .unwrap();
    assert!(session.bind_plakous_locator(other_branch).is_err());
    assert_eq!(session.plakous_locator, Some(locator(4)));
}

#[test]
fn openrouter_persisted_effort_is_closed_and_never_defaulted() {
    let mut session = bound_session("/work/openrouter-effort");
    session.account = Some("openrouter".into());
    session.model = Some("vendor/model".into());
    session.composition = ProductCompositionSelection::Direct;
    session.target_selection = openrouter_target();
    session.effort = Some("future-default".into());
    assert!(session.validate_for_store().is_err());

    session.effort = Some("high".into());
    assert!(session.validate_for_store().is_ok());
}

#[test]
fn stored_api_binding_is_readable_without_current_model_enrollment() {
    let mut session = bound_session("/work/historical-api");
    session.account = Some("openai".into());
    session.model = Some("historical-model-not-in-this-build".into());
    session.effort = Some("high".into());
    session.composition = ProductCompositionSelection::NativePtc;
    session.target_selection = ProductTargetSelection::Platform {
        provider: crate::binding::PlatformProvider::OpenAi,
    };
    session.validate_for_store().unwrap();
    assert_eq!(
        session.model.as_deref(),
        Some("historical-model-not-in-this-build")
    );
}

fn switch_selection(pending: bool) -> Box<SwitchSelection> {
    let snapshot = crate::orchestration::OrchestrationSnapshot::freeze(
        crate::orchestration::RosterInput::unpinned("grok-target"),
    );
    Box::new(SwitchSelection {
        rebind: *rebind_selection("grok-target"),
        profile: "grok-default".into(),
        root: RootProfile {
            root_template: Some("lead".into()),
            working_set: vec!["worker".into()],
        },
        roster_digest: snapshot.digest.clone(),
        pending_snapshot: pending.then(|| serde_json::to_string(&snapshot).unwrap()),
    })
}

/// A fork is a new node in a tree of its own; a rebind takes
/// the node of the tree it compiles against (its own, or its source's with
/// sub-agents off); a clear compiles against a tree of its own. A switch, decoded today,
/// continues the node and records the profile only on a virtual tree.
#[test]
fn each_successor_records_the_node_of_the_tree_it_compiles_against() {
    let mut source = bound_session("/work/nodes");
    source.title_override = Some("source label".into());
    source.archived = true;
    assert_eq!(source.node, SessionNode::genesis(&source.id));
    assert_eq!(
        (source.root_profile.as_ref(), source.runner.as_ref()),
        (None, None)
    );
    let fork = successor_from_fact(
        &source,
        "plak:fork:node",
        locator(3),
        SessionWorkflowKind::Fork,
        branch_locator(8, 9),
    );
    assert_eq!(fork.node, SessionNode::genesis(&fork.id));
    let rebind = successor_from_fact(
        &source,
        "plak:rebind:node",
        locator(3),
        SessionWorkflowKind::Rebind(rebind_selection("grok-target")),
        branch_locator(10, 11),
    );
    assert_eq!(rebind.node, SessionNode::genesis(&rebind.id));
    let mut staying = rebind_selection("grok-target");
    staying.own_tree = None;
    let stayed = successor_from_fact(
        &source,
        "plak:rebind:stays",
        locator(3),
        SessionWorkflowKind::Rebind(staying),
        branch_locator(14, 15),
    );
    assert_eq!(stayed.node, source.node);
    let clear = source
        .new_session_from_request(&projector::WorkflowRequest {
            operation_id: "plak:new:node".into(),
            kind: SessionWorkflowKind::New,
            source_locator: locator(3),
        })
        .unwrap();
    assert_eq!(clear.node, SessionNode::genesis(&clear.id));
    for session in [&fork, &rebind, &stayed, &clear] {
        assert_eq!(session.title_override, None);
        assert!(!session.archived);
    }
    for pending in [false, true] {
        let selection = switch_selection(pending);
        let switched = successor_from_fact(
            &source,
            &format!("plak:switch:node-{pending}"),
            locator(3),
            SessionWorkflowKind::Switch(selection.clone()),
            branch_locator(12, 13),
        );
        assert_eq!(switched.node, source.node);
        assert_eq!(switched.title_override, None);
        assert!(!switched.archived);
        assert_eq!(switched.model.as_deref(), Some("grok-target"));
        assert_eq!(switched.root_profile, Some(selection.root.clone()));
    }
}

#[test]
fn a_switch_selection_round_trips_and_refuses_bytes_outside_its_digest() {
    for pending in [false, true] {
        let selection = switch_selection(pending);
        let encoded = selection.encode().unwrap();
        assert_eq!(SwitchSelection::decode(&encoded).unwrap(), *selection);
        let (_, stored) = encode_workflow_kind(&SessionWorkflowKind::Switch(selection));
        assert_eq!(stored.as_deref(), Some(encoded.as_str()));
    }
    let mut other_digest = switch_selection(true);
    other_digest.roster_digest = "0".repeat(64);
    assert!(other_digest.encode().is_err());
    let mut not_hex = switch_selection(false);
    not_hex.roster_digest = "digest".into();
    assert!(not_hex.encode().is_err());
    let mut edited = switch_selection(true);
    edited.pending_snapshot = edited.pending_snapshot.map(|bytes| {
        bytes.replacen(
            "\"workspace_idle_seconds\":",
            "\"workspace_idle_seconds\":1",
            1,
        )
    });
    assert!(edited.encode().is_err());
    let encoded = switch_selection(false).encode().unwrap();
    let extra = encoded.replacen('{', "{\"extra\":1,", 1);
    assert!(SwitchSelection::decode(&extra).is_err());
    let no_profile = encoded.replacen("\"profile\":\"grok-default\",", "", 1);
    assert!(SwitchSelection::decode(&no_profile).is_err());
}

/// `own_tree` is a required key of a stored rebind selection: absent is
/// refused, `null` is a rebind that stays on its source's tree.
#[test]
fn a_rebind_selection_requires_its_own_tree_key() {
    let mut selection = rebind_selection("grok-target");
    selection.own_tree = None;
    let encoded = serde_json::to_value(&*selection).unwrap();
    assert!(encoded["own_tree"].is_null());
    assert_eq!(
        serde_json::from_value::<RebindSelection>(encoded.clone()).unwrap(),
        *selection
    );
    let mut absent = encoded;
    absent.as_object_mut().unwrap().remove("own_tree");
    assert!(serde_json::from_value::<RebindSelection>(absent).is_err());
}
