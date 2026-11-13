use super::*;

#[test]
fn escalation_is_available_before_cleanup_finishes_and_clean_completion_stays_clean() {
    let clean = HostWorkerHandle::spawn(|| Ok(()));
    assert_eq!(clean.join().unwrap().unwrap(), HostQuitOutcome::Clean);
    let (selected, selection) = std::sync::mpsc::channel();
    let (release, cleanup) = std::sync::mpsc::channel();
    let worker = HostWorkerHandle::spawn_with_receipt(move |receipt, _steps| {
        receipt.escalate();
        selected.send(()).unwrap();
        cleanup.recv().unwrap();
        Ok(())
    });
    selection.recv().unwrap();
    assert!(!worker.is_finished());
    assert_eq!(worker.quit_outcome(), HostQuitOutcome::Escalated);
    release.send(()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap(), HostQuitOutcome::Escalated);
}
