use super::accepted_duplicate_consumed;
use crate::sessions;

#[allow(clippy::unwrap_used, reason = "test assertions")]
#[test]
fn live_steer_skips_intent_reads_and_preserves_typed_store_refusals() {
    use super::RootLedger;
    use crate::driver::ledger::{
        BatchRule, CommandIntent, CommandKind, CommandLedger, LedgerError, Settlement,
    };
    use crate::error::StoreRefusal;
    use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
    use crate::session_store::{MemoryStore, SessionRecordStore as _};

    struct RefuseSteer;
    impl StoreFaults for RefuseSteer {
        fn before(
            &self,
            call: StoreCall,
            _: Option<&sessions::SessionRecord>,
        ) -> Result<(), String> {
            assert!(
                call != StoreCall::LoadIntents,
                "a live steer must not pre-read intents"
            );
            Ok(())
        }

        fn refusal(&self, call: StoreCall) -> Option<StoreRefusal> {
            matches!(
                call,
                StoreCall::SaveSubmission | StoreCall::DiscardSubmission
            )
            .then_some(StoreRefusal::StorageFull)
        }
    }

    let rows = crate::runtime::test_kit::MemoryRows::new();
    let inner = std::sync::Arc::new(MemoryStore::new());
    let store = FaultStore::new(inner.clone(), RefuseSteer);
    let mut session = sessions::SessionRecord::new(std::path::PathBuf::from("workspace"));
    inner.save(&mut session).unwrap();
    let before = session.clone();
    let item = sessions::QueueItem::new("steer");
    let command = format!("plak.send-now.{}.{}", item.id, item.revision);
    let result = RootLedger::new(&rows, &store, &mut session, &item).prepare(CommandIntent {
        command_id: &command,
        kind: CommandKind::RootSteer("00000000-0000-4000-8000-000000000001".parse().unwrap()),
        base_text: "steer",
        batch: BatchRule::None,
    });
    assert!(
        matches!(result, Err(LedgerError::Store(error)) if error.refusal() == Some(StoreRefusal::StorageFull))
    );
    for settlement in [Settlement::Consumed, Settlement::Refused { pause: None }] {
        let result =
            RootLedger::new(&rows, &store, &mut session, &item).settle(&command, settlement);
        assert!(
            matches!(result, Err(LedgerError::Store(error)) if error.refusal() == Some(StoreRefusal::StorageFull))
        );
    }
    assert_eq!(session, before);
    assert_eq!(inner.load(&session.id).unwrap(), before);
    assert_eq!(
        inner.load_intents(&session.id).unwrap().submissions,
        Vec::new()
    );
}

#[allow(clippy::unwrap_used, reason = "test assertions")]
#[test]
fn accepted_duplicate_settles_from_the_fact_or_the_retained_feed_floor() {
    let mut views = sessions::projector::SessionViews::empty();
    for retained_from in [None, Some(0)] {
        views.state.retained_from = retained_from;
        assert!(accepted_duplicate_consumed(&views, "command").is_err());
    }
    views.state.retained_from = Some(7);
    assert!(accepted_duplicate_consumed(&views, "command").unwrap());
    views.state.deliveries.insert(
        "command".to_owned(),
        sessions::projector::Delivery::Accepted,
    );
    assert!(!accepted_duplicate_consumed(&views, "command").unwrap());
    views.state.retained_from = None;
    assert!(!accepted_duplicate_consumed(&views, "command").unwrap());
}
