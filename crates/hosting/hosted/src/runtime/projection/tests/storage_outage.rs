use super::*;
use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

type Interrupt = Arc<dyn Fn() -> bool + Send + Sync>;

struct ParkedProduct {
    full: AtomicBool,
    parked: AtomicBool,
    interrupt: Mutex<Option<Interrupt>>,
    site: StoreCall,
}

impl StoreFaults for Arc<ParkedProduct> {
    fn set_storage_interrupt(&self, probe: Interrupt) {
        *self.interrupt.lock().unwrap() = Some(probe);
    }
    fn storage_wait_cancelled(&self) -> bool {
        self.interrupt
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|probe| probe())
    }
    fn storage_unavailable(&self) -> bool {
        self.parked.load(Ordering::Acquire)
    }
    fn before(
        &self,
        _: StoreCall,
        _: Option<&crate::sessions::SessionRecord>,
    ) -> Result<(), String> {
        Ok(())
    }
    fn before_write(
        &self,
        call: StoreCall,
        _: Option<&crate::sessions::SessionRecord>,
    ) -> Result<(), crate::session_store::RecordStoreError> {
        if self.full.load(Ordering::Acquire)
            && (call == self.site || self.parked.load(Ordering::Acquire))
        {
            self.parked.store(true, Ordering::Release);
            while !self.storage_wait_cancelled() {
                std::thread::park_timeout(Duration::from_millis(10));
            }
            return Err(crate::session_store::RecordStoreError::refused(
                crate::error::StoreRefusal::StorageFull,
                "injected capacity outage",
            ));
        }
        Ok(())
    }
}

#[test]
fn interrupted_product_write_keeps_host_alive_queue_identity_and_clean_quit() {
    for site in [StoreCall::Save, StoreCall::SaveSubmission] {
        assert_interrupted_product_write(site);
    }
}

fn assert_interrupted_product_write(site: StoreCall) {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.surface = None;
    let store = Arc::new(MemoryStore::new());
    let queued = crate::sessions::QueueItem::new("retained queue identity");
    config.session.pending_queue.push(queued.clone());
    config.session.queue_drain_state = crate::sessions::QueueDrainState::RestoredPaused;
    store.save(&mut config.session).unwrap();
    let id = config.session.id.clone();
    let faults = Arc::new(ParkedProduct {
        full: AtomicBool::new(false),
        parked: AtomicBool::new(false),
        interrupt: Mutex::new(None),
        site,
    });
    config.store = Arc::new(FaultStore::new(store.clone(), faults.clone()));
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    send_attached_action(
        &actions,
        UserAction::Steer("confirm host loop readiness".into()),
    );
    loop {
        if matches!(
            events.recv_timeout(Duration::from_secs(10)).unwrap(),
            HostEvent::Status { ref detail, .. } if matches!(detail, Some(plakous_host_protocol::status::HostStatusDetail::NoActiveTurn))
        ) {
            break;
        }
    }
    faults.full.store(true, Ordering::Release);
    send_attached_action(
        &actions,
        if site == StoreCall::SaveSubmission {
            UserAction::ResumeQueueDrain
        } else {
            UserAction::Prompt("prompt parked before acceptance".into())
        },
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !faults.parked.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "host reached the product write, controls={}, finished={}, events={:?}",
            site == StoreCall::WriteHostControls,
            worker.is_finished(),
            events.try_iter().collect::<Vec<_>>()
        );
        std::thread::yield_now();
    }
    send_attached_action(&actions, UserAction::Interrupt);
    send_attached_action(
        &actions,
        UserAction::Extension(crate::runtime::test_kit::ProbeIntent::InspectUsage),
    );
    let mut seen = Vec::new();
    loop {
        let event = events
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!(
                    "host response: {error}; finished={}; events={seen:?}",
                    worker.is_finished()
                )
            });
        if matches!(event, HostEvent::Row(TranscriptRow::Notice(ref notice)) if notice.text == crate::runtime::surface::SurfaceRefusal::NoHandler.note())
        {
            break;
        }
        seen.push(event);
    }
    assert!(!worker.is_finished(), "interrupt is not a host failure");
    assert_eq!(
        store.load(&id).unwrap().pending_queue.as_slice(),
        std::slice::from_ref(&queued)
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    assert_eq!(store.load(&id).unwrap().pending_queue, [queued]);
}
