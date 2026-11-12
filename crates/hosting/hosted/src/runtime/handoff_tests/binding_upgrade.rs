//! A planned same-store handoff never authorizes a different frozen binding.

use super::*;
use crate::runtime::root_loss_open_tests::Lost;

#[test]
fn same_key_restart_with_changed_binding_neither_carries_nor_consumes_handoff() {
    for busy in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let lost = if busy {
            release_a_running_root_turn(root.path(), Release::Handoff, Cancel::Prompt, |config| {
                config
            })
        } else {
            let (config, evidence) = scripted_host_with(root.path(), script(serde_json::json!([])));
            let mut lost = Lost {
                store: Arc::clone(&config.store),
                state_roots: config.state_roots.clone(),
                id: config.session.id.clone(),
                evidence,
                seen: Vec::new(),
            };
            let (actions, events, worker) = spawn_host_worker(config).unwrap();
            send_attached_action(&actions, UserAction::Prompt("save the idle session".into()));
            wait_for(&events, &mut lost.seen, "the initial turn", turn_completed);
            wait_for(&events, &mut lost.seen, "idle before handoff", idle);
            actions.request_handoff_and_release();
            worker.join().unwrap().unwrap();
            lost
        };
        let original = lost.store.load(&lost.id).unwrap();
        let replacement = serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "adapter_profile": "offline.binding-upgrade",
            "steps": []
        }))
        .unwrap();
        let (changed, _) = scripted_host_with(root.path(), Arc::new(replacement));
        let mut config = next_host(
            root.path(),
            &lost,
            serde_json::json!([]),
            Arc::clone(&lost.store),
        );
        // A real frozen adapter change is the control: unlike a per-request
        // host execution identity, it must refuse before continuation.
        config.runtime_factory = changed.runtime_factory;
        let Err(failure) = spawn_host_worker(config) else {
            panic!("a changed binding must not open through a handoff");
        };
        assert_eq!(
            failure.host_failure().cause,
            plakous_host_protocol::status::AttachFailure::BindingChanged
        );
        assert!(
            !failure.is_recovery_required(),
            "binding repair is not a recovery fence"
        );
        let retained = lost.store.load(&lost.id).unwrap();
        assert_eq!(retained.plakous_locator, original.plakous_locator);
        assert_eq!(retained.id, original.id);
        let intents = lost.store.load_intents(&lost.id).unwrap();
        assert_eq!(intents.root_interrupts.len(), usize::from(busy));
        assert_eq!(intents.root_continuations.len(), 0);
        assert_eq!(dispatches(&lost.evidence), 1);

        // Rolling back the binding can still consume the unspent handoff once.
        let config = next_host(
            root.path(),
            &lost,
            serde_json::json!([{"output":{"text":"continued"}}]),
            Arc::clone(&lost.store),
        );
        let (actions, events, worker) = spawn_host_worker(config).unwrap();
        let mut seen = Vec::new();
        wait_for(
            &events,
            &mut seen,
            "reopen under original binding",
            if busy { turn_completed } else { idle },
        );
        actions.request_handoff_and_release();
        worker.join().unwrap().unwrap();
        assert_eq!(dispatches(&lost.evidence), if busy { 2 } else { 1 });
        assert_eq!(
            lost.store
                .load_intents(&lost.id)
                .unwrap()
                .root_continuations
                .len(),
            usize::from(busy)
        );
    }
}
