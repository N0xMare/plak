use super::*;
use std::sync::atomic::AtomicBool;

#[test]
fn host_step_drain_only_an_exact_attached_interrupt_can_withdraw_preparation() {
    let (host, _) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let pause = host
        .pause_admissions_for_upgrade(Arc::clone(&cancelled))
        .unwrap();
    for observed in [attachment + 1, attachment] {
        request(
            &mut connection,
            2 + observed,
            Op::Intent {
                attachment: observed,
                intent: SessionIntent::Interrupt,
                client_id: None,
                blobs: Vec::new(),
            },
        );
        assert_eq!(cancelled.load(Ordering::Acquire), observed == attachment);
    }
    assert!(pause.commit().is_err());
}

#[test]
fn host_step_drain_stop_withdraws_preparation_but_never_crosses_commit() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    let cancelled = Arc::new(AtomicBool::new(false));
    let pause = host
        .pause_admissions_for_upgrade(Arc::clone(&cancelled))
        .unwrap();
    request(
        &mut connection,
        1,
        Op::StopSession {
            session: "absent".into(),
        },
    );
    assert!(!cancelled.load(Ordering::Acquire));
    request(
        &mut connection,
        2,
        Op::StopSession {
            session: "s1".into(),
        },
    );
    assert!(cancelled.load(Ordering::Acquire));
    assert!(pause.commit().is_err());
    assert_eq!(fake.0.dormant_stops.load(Ordering::SeqCst), 1);
    drop(pause);
    let cancelled = Arc::new(AtomicBool::new(false));
    let pause = host
        .pause_admissions_for_upgrade(Arc::clone(&cancelled))
        .unwrap();
    pause.commit().unwrap();
    request(
        &mut connection,
        3,
        Op::StopSession {
            session: "s1".into(),
        },
    );
    assert!(!cancelled.load(Ordering::Acquire));
    assert!(matches!(
        outcome(&out.take(), 3),
        Outcome::Refused(RequestRefusal::SessionUnavailable { .. })
    ));
    assert_eq!(fake.0.dormant_stops.load(Ordering::SeqCst), 1);
    drop(pause);
    request(
        &mut connection,
        4,
        Op::StopSession {
            session: "s1".into(),
        },
    );
    assert_eq!(fake.0.dormant_stops.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn host_step_drain_busy_slot_retries_instead_of_refusing_the_coordinator() {
    let (host, _) = host(&[]);
    let slot = host.slot("s1");
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        slot.mutate_lifecycle("s1", false, move || {
            entered.send(()).unwrap();
            let _ = released.recv();
            Ok(())
        })
    });
    entering.recv_timeout(Duration::from_secs(5)).unwrap();
    let pause = host.pause_admissions().unwrap();
    let cancelled = AtomicBool::new(false);
    let monitor = StepDrainMonitor {
        cancelled: &cancelled,
        progress: &|_| Ok(()),
    };
    let mut draining = Box::pin(host.drain_steps(
        &pause,
        Some(Instant::now()),
        Duration::from_secs(2),
        &monitor,
        Vec::new(),
    ));
    assert!(
        std::future::poll_fn(|context| {
            std::task::Poll::Ready(Future::poll(draining.as_mut(), context).is_pending())
        })
        .await
    );
    release.send(()).unwrap();
    let held = draining.await.unwrap();
    worker.join().unwrap().unwrap();
    drop(held);
    drop(pause);
    assert!(host.pause_admissions().is_ok());
}

#[tokio::test]
async fn host_step_drain_refuses_foreign_pause_without_touching_either_host() {
    let (first, _) = host(&[]);
    let (second, _) = host(&[]);
    let pause = first.pause_admissions().unwrap();
    assert!(
        second
            .drain_steps(
                &pause,
                Some(Instant::now()),
                Duration::ZERO,
                &StepDrainMonitor {
                    cancelled: &AtomicBool::new(false),
                    progress: &|_| Ok(())
                },
                Vec::new()
            )
            .await
            .is_err()
    );
    assert!(second.pause_admissions().is_ok());
    drop(pause);
    assert!(first.pause_admissions().is_ok());
}

#[tokio::test]
async fn host_step_drain_timeout_releases_its_worker_generation_before_admissions_reopen() {
    let (host, _) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    assert!(matches!(
        outcome(&out.take(), 1),
        Outcome::Ok(Answer::Attached { .. })
    ));
    let pause = host.pause_admissions().unwrap();
    assert!(
        host.drain_steps(
            &pause,
            Some(Instant::now()),
            Duration::ZERO,
            &StepDrainMonitor {
                cancelled: &AtomicBool::new(false),
                progress: &|_| Ok(())
            },
            Vec::new()
        )
        .await
        .is_err()
    );
    for slot in host.slots() {
        assert!(matches!(
            slot.pause_steps().unwrap(),
            super::super::super::slot::StepOwnerHold::Held(_)
        ));
    }
    drop(pause);
    request(&mut connection, 2, Op::Ping);
    assert_eq!(outcome(&out.take(), 2), Outcome::Ok(Answer::Pong));
    let failures = host.hand_off_all(Duration::from_secs(2));
    assert_eq!(failures.len(), 0, "{failures:?}");
}
