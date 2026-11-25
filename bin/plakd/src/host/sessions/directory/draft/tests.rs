#![allow(clippy::unwrap_used, reason = "directory fixtures")]

use super::*;
use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptRow};
use plakous_host_protocol::wire::SessionResidency;
use plakous_hosted::host_service::{KeptActivity, KeptDirectoryAttention};
use plakous_hosted::sessions::projector::{SessionViews, ViewWrites};
use std::sync::Arc;

#[test]
fn draft_differences_preserve_the_exact_named_session_tier() {
    use plakous_host_protocol::views::ProfileDifference as Old;
    use v13::{ProfileDifference as New, ServiceTier};

    let fast = service_tier(Some("fast"));
    assert_eq!(fast, Some(ServiceTier::Fast));
    assert_eq!(fast, service_tier(Some("priority")));
    let ultrafast = service_tier(Some("ultrafast"));
    assert_eq!(
        differences(&[], ultrafast, ultrafast != fast),
        vec![New::RootServiceTier {
            session: Some(ServiceTier::Ultrafast),
        }]
    );
    assert_eq!(
        differences(&[Old::RootFast { session: false }], None, true),
        vec![New::RootServiceTier { session: None }]
    );
}

#[test]
fn preview_redacts_and_removes_markup_before_the_utf8_cut() {
    let input = "<b>**Title**</b>\x1b[31m\r\n[link](https://invalid.example/)\u{2028}\nAuthorization: Bearer example-value\n";
    let rendered = preview(input.to_owned()).unwrap();
    assert!(rendered.as_str().starts_with("Title link"));
    assert!(!rendered.as_str().contains("example-value"));
    assert_eq!(
        preview(format!("{}é!", "a".repeat(159))).unwrap().as_str(),
        "a".repeat(159)
    );
}

#[test]
fn publication_and_list_project_the_same_draft_row_and_clear_live_attention() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let mut store = crate::store::ProductStore::open(&paths).unwrap();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_owned());
    store.commit(&mut record).unwrap();
    let tree = Arc::new(crate::store::tree::open_tree_store(&paths).unwrap());
    let directory = Directory::start(&paths, tree).unwrap();
    let (observed, slot_changes) = watch::channel(0_u64);
    let mut listener = Watch::new(directory.changes(), slot_changes);
    let mut slots = BTreeMap::new();
    let read = |slots: &BTreeMap<String, DirectorySlot>| {
        directory
            .project_draft_rows(None, None, false, &[], slots)
            .remove(0)
    };
    let before = read(&slots);
    assert_eq!(
        listener.refresh(|| vec![read(&slots)]),
        Some(vec![before.clone()])
    );
    assert_eq!(listener.refresh(|| vec![read(&slots)]), None);
    assert!(before.preview.is_none());
    assert_eq!(before.cost_root, v13::Cost::Unknown);
    let empty = SessionViews::empty();
    let mut current = empty.clone();
    current.rows.insert(
        RowKey::new(1, Lane::Runtime, 0),
        TranscriptRow::user("Initial prompt"),
    );
    store
        .commit_projection_views(
            &record.id,
            &ViewWrites::between(&empty, &current),
            &current.state,
        )
        .unwrap();
    directory.settle();
    assert_eq!(
        listener.refresh(|| vec![read(&slots)]),
        Some(vec![read(&slots)])
    );
    assert_eq!(read(&slots).preview.unwrap().as_str(), "Initial prompt");
    let before = current.clone();
    current.rows.insert(
        RowKey::new(2, Lane::Runtime, 0),
        TranscriptRow::assistant("Latest answer"),
    );
    current.rows.insert(
        RowKey::new(3, Lane::Runtime, 0),
        TranscriptRow::user("New prompt"),
    );
    store
        .commit_projection_views(
            &record.id,
            &ViewWrites::between(&before, &current),
            &current.state,
        )
        .unwrap();
    directory.settle();
    slots.insert(record.id.clone(), resident_slot());
    let listed = read(&slots);
    assert_eq!(
        listener.refresh(|| vec![read(&slots)]),
        Some(vec![listed.clone()])
    );
    assert_eq!(listed.preview.unwrap().as_str(), "Latest answer");
    assert!(
        listed
            .attention
            .contains(&v13::AttentionReason::ApprovalNeeded { since: 3 })
    );
    assert!(
        listed
            .attention
            .contains(&v13::AttentionReason::CredentialWait { since: 5 })
    );
    assert!(
        listed
            .attention
            .iter()
            .any(|reason| matches!(reason, v13::AttentionReason::ContextCapacity { .. }))
    );
    slots.clear();
    observed.send_replace(1);
    assert_eq!(
        listener.refresh(|| vec![read(&slots)]),
        Some(vec![read(&slots)])
    );
    assert!(!read(&slots).attention.iter().any(|reason| matches!(
        reason,
        v13::AttentionReason::ApprovalNeeded { .. }
            | v13::AttentionReason::CredentialWait { .. }
            | v13::AttentionReason::ContextCapacity { .. }
    )));
}

fn resident_slot() -> DirectorySlot {
    DirectorySlot {
        residency: SessionResidency::Resident,
        open_failed: false,
        resident: Some((
            KeptActivity::default(),
            KeptDirectoryAttention {
                approval: Some(3),
                credential: Some(5),
                runtime: Some(RuntimeReason::ContextCapacity),
                ..KeptDirectoryAttention::default()
            },
        )),
    }
}

#[test]
fn categories_are_deduplicated_and_keep_only_current_occurrences() {
    let mut previous = BTreeMap::new();
    let mut active = Candidates::default();
    active.add(Kind::RecoveryRequired, None);
    active.add(Kind::UsageLimit("openai/work".into()), Some(4));
    active.add(Kind::UsageLimit("openai/work".into()), Some(8));
    active.add(Kind::InterruptedByUpgrade, None);
    let first = publish_attention(&mut previous, active.clone(), || 9);
    assert_eq!(
        first,
        vec![
            v13::AttentionReason::RecoveryRequired { since: 9 },
            v13::AttentionReason::UsageLimit {
                account: "openai/work".into(),
                since: 4
            },
            v13::AttentionReason::InterruptedByUpgrade { since: 9 },
        ]
    );
    assert_eq!(
        first,
        publish_attention(&mut previous, active.clone(), || 11)
    );
    assert_eq!(
        publish_attention(&mut previous, Candidates::default(), || 12),
        Vec::<v13::AttentionReason>::new()
    );
    assert_eq!(
        publish_attention(&mut previous, active, || 17)[0],
        v13::AttentionReason::RecoveryRequired { since: 17 }
    );
}
