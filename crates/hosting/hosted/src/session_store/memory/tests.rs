use super::*;
use crate::sessions::projector::{Successor, WorkflowRequest};
use plakous::sessions::feed::{FeedBranchTransition, FeedLocator, FeedSeq};

#[test]
fn a_deleted_clear_reservation_still_withdraws_once_in_memory() {
    let store = MemoryStore::new();
    let mut source = SessionRecord::new("/tmp/plak-memory-clear-deletion".into());
    let locator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003".parse().unwrap();
    source.bind_plakous_locator(locator).unwrap();
    store.save(&mut source).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:clear:deleted-reservation".into(),
        source_locator: locator,
        kind: SessionWorkflowKind::Clear,
    };
    store
        .submit_workflow_request(&mut source, &request)
        .unwrap();
    {
        let mut state = store.state.lock().unwrap();
        state.records.remove(&request.successor_product_id());
        state.intents.remove(&request.successor_product_id());
    }
    let empty = SessionViews::empty();
    store
        .commit_projection_views(
            &source.id,
            &ViewWrites::between(&empty, &empty),
            &empty.state,
        )
        .unwrap();
    assert_eq!(
        store
            .withdraw_unadmitted_workflows(&mut source)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store.withdraw_unadmitted_workflows(&mut source).unwrap(),
        [] as [plakous_host_protocol::views::transcript::TranscriptRow; 0]
    );
    let intents = store.load_intents(&source.id).unwrap();
    assert_eq!(intents.workflow_requests, [] as [WorkflowRequest; 0]);
    assert_eq!(intents.notices.len(), 1);
    assert!(store.load(&request.successor_product_id()).is_err());
}

#[test]
fn deleted_successor_is_not_resurrected_by_memory_projection() {
    let store = MemoryStore::new();
    let mut source = SessionRecord::new("/tmp/plak-memory-successor".into());
    let locator: plakous::sessions::SessionLocator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003".parse().unwrap();
    source.bind_plakous_locator(locator).unwrap();
    store.save(&mut source).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:fork:memory-deletion".into(),
        source_locator: locator,
        kind: SessionWorkflowKind::Fork,
    };
    store
        .submit_workflow_request(&mut source, &request)
        .unwrap();
    let mut views = SessionViews::empty();
    views.cursor = FeedSeq::new(1);
    let mut target_views = views.clone();
    target_views.state.head = Some(FeedLocator {
        session_id: locator.session_id(),
        branch_id: "00000000-0000-4000-8000-000000000004".parse().unwrap(),
        checkpoint_id: locator.checkpoint_id(),
    });
    let mut writes = ViewWrites::between(&SessionViews::empty(), &views);
    writes.successors.push(Successor {
        operation_id: Some(request.operation_id.clone()),
        transition: FeedBranchTransition::Fork,
        target: target_views.state.head.unwrap(),
        published_seq: views.cursor,
        created_at_unix: 42,
        seed: Some(target_views.clone()),
    });
    store
        .commit_projection_views(&source.id, &writes, &views.state)
        .unwrap();
    let target = request.successor_product_id();
    assert_eq!(store.load(&target).unwrap().created_at_unix, 42);
    assert_eq!(
        store.load_views(&target).unwrap().as_ref(),
        Some(&target_views)
    );
    {
        let mut state = store.state.lock().unwrap();
        state.records.remove(&target);
        state.views.remove(&target);
        state.intents.remove(&target);
        state.views.remove(&source.id);
    }
    assert_eq!(
        store
            .commit_projection_views(&source.id, &writes, &views.state)
            .unwrap(),
        ProjectionCommit::Applied
    );
    assert!(
        store.load(&target).is_err(),
        "deleted successor resurrected"
    );
    assert!(store.load_views(&target).unwrap().is_none());
}
