use super::*;
use plakous_hosted::sessions::projector::{
    RootInterrupt, RootInterruptCause, RootLossIntent, RootStopAnchor,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn a_stop_ends_the_start_scan_with_an_unready_continuation() {
    let (_temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let services = Services::for_home(paths.clone());
    let mut store = crate::store::ProductStore::open(&paths).unwrap();
    let mut record = plakous_hosted::sessions::SessionRecord::new(paths.home().to_owned());
    store.commit(&mut record).unwrap();
    store
        .write_root_loss_at_cursor(
            &record.id,
            &RootLossIntent::Interrupt(RootInterrupt {
                anchor: RootStopAnchor::Turn {
                    turn_id: "handoff".into(),
                },
                cause: RootInterruptCause::Handoff,
            }),
        )
        .unwrap();
    assert!(sessions::continuation_owed(&services, &record.id).unwrap());
    let product = sessions::ProductSessions::new(&services, None, None).unwrap();
    let host = SessionHost::new(product, "scan-test", 1);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let scanning = Arc::new(AtomicBool::new(true));
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopped);
    scan(
        &host,
        vec![record.id.clone()],
        move || stop.load(Ordering::Acquire),
        &services,
        runtime.handle(),
        Arc::clone(&scanning),
    );
    stopped.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while scanning.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "Stop left the startup scan retrying"
        );
        std::thread::yield_now();
    }
    assert!(sessions::continuation_owed(&services, &record.id).unwrap());
    assert!(
        !paths.runtime_database().exists(),
        "the scan admitted a driver"
    );
}
