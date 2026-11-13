use super::*;
use crate::home::paths::PlakPaths;
use plakous::sessions::SessionLocator;
use plakous::sessions::feed::{FeedLocator, FeedSeq};
use plakous_hosted::sessions::projector::{
    LineageStep, Submission, SubmissionKind, pending_command_ids,
};
use plakous_hosted::sessions::{QueueItem, SessionRecord};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn dormant_product_sqlite_seed_is_atomic_once_only_and_rebuildable_after_restart() -> TestResult {
    let root = tempfile::tempdir()?;
    let source_paths = PlakPaths::explicit(root.path().join("source"));
    let paths = PlakPaths::explicit(root.path().join("target"));
    let mut source = ProductStore::open(&source_paths)?;
    let locator: SessionLocator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003".parse()?;
    let target: SessionLocator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000004".parse()?;
    let mut record = SessionRecord::new(root.path().join("workspace"));
    record.bind_plakous_locator(locator)?;
    source.commit(&mut record)?;
    let mut views = SessionViews::empty();
    views.cursor = FeedSeq::new(20);
    views.state.runtime_session_id = Some(locator.session_id());
    views.state.lineage.push(LineageStep {
        branch_id: locator.branch_id(),
        from_seq: 1,
    });
    views.state.head = Some(FeedLocator {
        session_id: locator.session_id(),
        branch_id: locator.branch_id(),
        checkpoint_id: locator.checkpoint_id(),
    });
    assert_eq!(
        source.commit_projection_views(
            &record.id,
            &ViewWrites::between(&SessionViews::empty(), &views),
            &views.state
        )?,
        ProjectionCommit::Applied
    );
    record.pending_queue.push(QueueItem {
        id: uuid::Uuid::new_v4().to_string(),
        revision: 1,
        text: "owed input".into(),
        submitted_at_ms: 12,
        images: std::sync::Arc::default(),
    });
    let submission = Submission {
        command_id: "move:owed".into(),
        kind: SubmissionKind::RootStart,
        queue_item_id: record.pending_queue[0].id.clone(),
        queue_item_revision: 1,
        target_turn_id: None,
        canonical_command: "{}".into(),
        submitted_at_ms: 12,
    };
    source.commit_submission(&mut record, &submission)?;
    let intents = source.load_intents(&record.id)?;
    let acknowledgement = acknowledgement();
    source.acknowledge_agent_attempt(&record.node.tree_id, &acknowledgement, 42)?;
    let seed = DormantSession::prepare(record.clone(), views, intents.clone(), FeedSeq::new(20))?
        .with_identity_claims(source.move_identity_claims(&record.id)?)?
        .with_acknowledgements(source.move_acknowledgements(&record.node.tree_id)?)?
        .with_plan_authority("retained_fork_namespace".into())?
        .reconstruct(target)?;
    let digest = "a".repeat(64);
    let mut store = ProductStore::open(&paths)?;
    store.install_move_seed("move", &digest, &seed)?;
    assert_eq!(
        store.agent_acknowledgements(&record.node.tree_id)?,
        [acknowledgement]
    );
    assert_eq!(
        store.move_acknowledgements(&record.node.tree_id)?,
        source.move_acknowledgements(&record.node.tree_id)?
    );
    assert_eq!(
        store.move_plan_authority(&record.id)?.as_deref(),
        Some("retained_fork_namespace")
    );
    let mut installed = store.load(&record.id)?;
    assert_eq!(installed.plakous_locator, Some(target));
    assert_eq!(installed.pending_queue, record.pending_queue);
    assert_eq!(store.load_intents(&record.id)?, intents);
    installed.title = Some("changed after install".into());
    store.commit(&mut installed)?;
    drop(store);
    let mut store = ProductStore::open(&paths)?;
    store.install_move_seed("move", &digest, &seed)?;
    assert_eq!(
        store.move_plan_authority(&record.id)?.as_deref(),
        Some("retained_fork_namespace")
    );
    assert_eq!(store.load(&record.id)?.title, installed.title);
    assert!(store.install_move_seed("other", &digest, &seed).is_err());
    rebuild_projection(&mut store, &record.id, &intents, &submission.command_id)?;
    drop(store);
    let mut store = ProductStore::open(&paths)?;
    let restored = store
        .load_views(&record.id)?
        .ok_or("missing rebuilt projection")?;
    assert_eq!(restored.cursor, FeedSeq::new(20));
    assert_eq!(
        restored.state.head.map(|head| head.checkpoint_id),
        Some(target.checkpoint_id())
    );
    assert_eq!(
        store.move_identity_claims(&record.id)?,
        source.move_identity_claims(&record.id)?
    );
    assert_corrupt_seed_refused(&mut store, &record.id)
}

fn acknowledgement() -> crate::surface_protocol::AgentAcknowledgement {
    crate::surface_protocol::AgentAcknowledgement {
        agent_id: "child".into(),
        attempt_id: "child-a1".into(),
        attention: "failed".into(),
        ended_at: Some(41),
    }
}

fn rebuild_projection(
    store: &mut ProductStore,
    id: &str,
    intents: &ProductIntents,
    command: &str,
) -> TestResult {
    store.delete_views(id)?;
    assert!(store.load_views(id)?.is_none());
    let mut base = store
        .move_projection_base(id)?
        .ok_or("missing semantic base")?;
    base.state.observe_retained_from(21);
    base.consume_before_retention(&store.load_intents(id)?);
    assert_eq!(pending_command_ids(&base, intents), [command]);
    assert_eq!(
        store.commit_projection_views(
            id,
            &ViewWrites::between(&SessionViews::empty(), &base),
            &base.state
        )?,
        ProjectionCommit::Applied
    );
    Ok(())
}

fn assert_corrupt_seed_refused(store: &mut ProductStore, id: &str) -> TestResult {
    store.connection.execute(
        "UPDATE session_move_seeds SET seed_digest=?1 WHERE session_id=?2",
        [&"b".repeat(64), id],
    )?;
    assert!(store.move_projection_base(id).is_err());
    Ok(())
}
