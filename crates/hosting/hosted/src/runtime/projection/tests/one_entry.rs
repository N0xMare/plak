use super::*;

#[test]
fn live_and_plain_open_enter_the_same_projector_and_all_view_writes_are_inside_it() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    super::super::probe::track(&id);
    run_prompts(config, &["live turn"]);
    let first = super::super::probe::take(&id);
    assert!(first.live > 0, "live loop bypassed the common entry");
    assert!(first.writes > 0, "live loop wrote no projected views");
    assert_eq!(first.release, 1, "release bypassed the common entry");

    // Ordinary stored-session startup, not a direct projector call.
    super::super::probe::track(&id);
    run_prompts(reopened(root.path(), &store, &id), &[]);
    let second = super::super::probe::take(&id);
    assert!(second.open > 0, "plain open bypassed the common entry");
    assert!(second.live > 0, "reopened host bypassed the common entry");
    assert!(second.writes > 0, "reopen did not write projected views");
    assert_eq!(
        second.release, 1,
        "reopen release bypassed the common entry"
    );
}

#[test]
fn a_view_writer_outside_the_common_entry_is_rejected_even_beside_an_active_pass() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["seed"]);
    let views = caught_up(root.path(), &store, &id);
    let writes = crate::sessions::projector::ViewWrites::between(&views, &views);
    super::super::probe::track(&id);
    let entry = super::super::probe::enter(&id);
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.commit_projection_views(&id, &writes, &views)
    }));
    assert!(refused.is_err(), "the probe allowed a bypass writer");
    assert_eq!(store.load_views(&id).unwrap(), Some(views));
    drop(entry);
    assert_eq!(super::super::probe::take(&id).writes, 0);
}
