use std::time::{Duration, Instant};

use plakous_host_protocol::{SessionIntent, SessionObservation};

use super::{RoundFace, round_face};
use crate::surface_protocol::{HostEvent, PlakExtension, SurfaceIntent, SurfaceObservation};

/// The face's next observation, within a liveness bound.
fn next(face: &RoundFace) -> HostEvent {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(event) = face.events.try_recv() {
            return event;
        }
        if let Some(unsent) = face.events.try_unsent() {
            panic!("the session did not take {unsent:?}");
        }
        assert!(Instant::now() < deadline, "no observation came");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A round's face over frames reaches its session as a local face: a
/// local-only intent is taken, the session's observation and the
/// launcher's lookup progress come back, and when the launcher lets the
/// face go the session is quit.
#[test]
fn a_round_face_speaks_frames_to_its_session() {
    let (actions, intents) = crate::surface_protocol::user_action_channel();
    let (observations, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
        while let Ok(delivery) = intents.recv() {
            match &*delivery {
                SessionIntent::Quit => break,
                SessionIntent::Extension(SurfaceIntent::CheckBuild) => {
                    observations
                        .send(SessionObservation::SetBusy(true))
                        .unwrap();
                }
                _ => {}
            }
        }
        Ok(())
    });
    let face = round_face((actions, events, worker), "s1").unwrap();
    face.actions
        .send(SessionIntent::Extension(SurfaceIntent::CheckBuild))
        .unwrap();
    assert_eq!(next(&face), SessionObservation::SetBusy(true));
    face.publisher
        .publish(HostEvent::Extension(SurfaceObservation::LookupStatus {
            text: Some("looking".into()),
        }));
    assert_eq!(
        next(&face),
        HostEvent::Extension(SurfaceObservation::LookupStatus {
            text: Some("looking".into()),
        })
    );
    let RoundFace {
        actions,
        events,
        mailbox,
        join,
        ..
    } = face;
    drop((actions, events, mailbox));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !join.is_finished() {
        assert!(Instant::now() < deadline, "the session was not quit");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(join.join().is_ok());
}

/// Waits for `join`'s worker to end and joins it.
fn joined(join: plakous_hosted::runtime::HostWorkerHandle) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !join.is_finished() {
        assert!(Instant::now() < deadline, "the session did not end");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(join.join().is_ok());
}

/// The client's first root attach is answered before the connection takes
/// anything from the session, so what the session said before the face
/// connected reaches the face: a row, which no attach snapshot keeps.
#[test]
fn what_the_session_said_before_the_face_connected_reaches_it() {
    let (actions, intents) = crate::surface_protocol::user_action_channel();
    let (observations, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    let early = SessionObservation::Row(
        plakous_host_protocol::views::transcript::TranscriptRow::user("said before the face"),
    );
    observations.send(early.clone()).unwrap();
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
        let _observations = observations;
        while let Ok(delivery) = intents.recv() {
            if matches!(&*delivery, SessionIntent::Quit) {
                break;
            }
        }
        Ok(())
    });
    let face = round_face((actions, events, worker), "s1").unwrap();
    assert_eq!(next(&face), early);
    let RoundFace {
        actions,
        events,
        mailbox,
        join,
        ..
    } = face;
    drop((actions, events, mailbox));
    joined(join);
}

/// What the TUI sent before the launcher's release is in the session's
/// mailbox before the release flag lands, though no send waited for its
/// verdict: the release waits for them.
#[test]
fn what_the_tui_sent_reaches_the_session_before_the_release() {
    const SENT: usize = 5;
    let (actions, intents) = crate::surface_protocol::user_action_channel();
    let (observations, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    let (counted, counts) = std::sync::mpsc::channel();
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
        let _observations = observations;
        let mut prompts = 0;
        while let Ok(delivery) = intents.recv() {
            match &*delivery {
                SessionIntent::Prompt(_) => prompts += 1,
                SessionIntent::Quit => {
                    // The release outranks queued work: count what is queued
                    // behind it too.
                    while let Ok(queued) = intents.try_recv() {
                        if matches!(&*queued, SessionIntent::Prompt(_)) {
                            prompts += 1;
                        }
                    }
                    let _ = counted.send(prompts);
                    break;
                }
                _ => {}
            }
        }
        Ok(())
    });
    let RoundFace {
        actions,
        events,
        mailbox,
        join,
        ..
    } = round_face((actions, events, worker), "s1").unwrap();
    for index in 0..SENT {
        actions
            .send(SessionIntent::Prompt(format!("sent {index}").into()))
            .unwrap();
    }
    mailbox.request_handoff_and_release();
    assert_eq!(counts.recv_timeout(Duration::from_secs(30)).unwrap(), SENT);
    drop((actions, events, mailbox));
    joined(join);
}

/// An observation the session publishes on its own, with nothing sent,
/// wakes the face's notifier: a parked TUI paints it without a poll.
#[test]
fn an_unsolicited_observation_wakes_the_face() {
    let (actions, intents) = crate::surface_protocol::user_action_channel();
    let (observations, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    let (publish, published) = std::sync::mpsc::channel::<()>();
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
        let _ = published.recv();
        observations
            .send(SessionObservation::SetBusy(true))
            .unwrap();
        while let Ok(delivery) = intents.recv() {
            if matches!(&*delivery, SessionIntent::Quit) {
                break;
            }
        }
        Ok(())
    });
    let face = round_face((actions, events, worker), "s1").unwrap();
    let (woke, woken) = std::sync::mpsc::channel();
    face.events.set_notify(move || {
        let _ = woke.send(());
    });
    publish.send(()).unwrap();
    woken
        .recv_timeout(Duration::from_secs(30))
        .expect("the face was not woken");
    assert_eq!(next(&face), SessionObservation::SetBusy(true));
    let RoundFace {
        actions,
        events,
        mailbox,
        join,
        ..
    } = face;
    drop((actions, events, mailbox));
    joined(join);
}

/// A session that ends on its own says its last observation to the face
/// before the connection ends.
#[test]
fn the_last_observation_before_the_session_ends_reaches_the_face() {
    let (actions, intents) = crate::surface_protocol::user_action_channel();
    let (observations, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    let (end, ended) = std::sync::mpsc::channel::<()>();
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
        let _intents = intents;
        let _ = ended.recv();
        observations
            .send(SessionObservation::SetBusy(true))
            .unwrap();
        Ok(())
    });
    let face = round_face((actions, events, worker), "s1").unwrap();
    end.send(()).unwrap();
    assert_eq!(next(&face), SessionObservation::SetBusy(true));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match face.events.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Ok(other) => panic!("after the session's end: {other:?}"),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline, "the connection did not end");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
    joined(face.join);
}

/// The launcher's handoff flag lands before the face it holds leaves: the
/// session takes the release as a planned handoff, never the face's
/// leaving as a plain quit first.
#[test]
fn a_handoff_lands_before_the_face_leaves() {
    let (actions, intents) = crate::surface_protocol::user_action_channel();
    let (observations, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    let (quit, quits) = std::sync::mpsc::channel();
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
        let _observations = observations;
        while let Ok(delivery) = intents.recv() {
            if matches!(&*delivery, SessionIntent::Quit) {
                let _ = quit.send(intents.planned_handoff());
                break;
            }
        }
        Ok(())
    });
    let RoundFace {
        actions,
        events,
        mailbox,
        join,
        ..
    } = round_face((actions, events, worker), "s1").unwrap();
    // The TUI is gone, as when it returns a reload request.
    drop((actions, events));
    std::thread::sleep(Duration::from_millis(200));
    mailbox.request_handoff_and_release();
    drop(mailbox);
    assert!(
        quits.recv_timeout(Duration::from_secs(30)).unwrap(),
        "the session quit before the handoff"
    );
    joined(join);
}

/// The face reads a transcript the connection lost as lost until its fresh
/// baseline: the session's mailbox dropped root content before the face
/// attached, so the attach asks for a baseline and the face waits for it.
#[test]
fn a_transcript_lost_before_the_attach_reads_as_lost() {
    let (actions, intents) = crate::surface_protocol::user_action_channel();
    let (observations, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    // Rows past the mailbox's bound are dropped as a baseline repays them.
    for index in 0..=plakous_host_protocol::limits::OBSERVATION_QUEUE_CAPACITY {
        let _ = observations.send(SessionObservation::Row(
            plakous_host_protocol::views::transcript::TranscriptRow::user(format!("row {index}")),
        ));
    }
    let (resynced, resyncs) = std::sync::mpsc::channel();
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
        let _observations = observations;
        while let Ok(delivery) = intents.recv() {
            match &*delivery {
                SessionIntent::ResyncTranscript { scope } => {
                    let _ = resynced.send(scope.clone());
                }
                SessionIntent::Quit => break,
                _ => {}
            }
        }
        Ok(())
    });
    let face = round_face((actions, events, worker), "s1").unwrap();
    assert_eq!(
        resyncs.recv_timeout(Duration::from_secs(30)).unwrap(),
        plakous_host_protocol::views::transcript::TranscriptScope::Root
    );
    let RoundFace {
        actions,
        events,
        mailbox,
        join,
        ..
    } = face;
    drop((actions, events, mailbox));
    joined(join);
}

/// Rows the TUI did not take in time are read as lost until the next root
/// baseline, so the TUI asks for one.
#[test]
fn rows_the_tui_could_not_hold_read_as_lost() {
    let (actions, intents) = crate::surface_protocol::user_action_channel();
    let (observations, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    let (publish, published) = std::sync::mpsc::channel::<()>();
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
        let _ = published.recv();
        for index in 0..4 * plakous_host_protocol::limits::OBSERVATION_QUEUE_CAPACITY {
            let _ = observations.send(SessionObservation::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::user(format!(
                    "row {index}"
                )),
            ));
            if index % 64 == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        while let Ok(delivery) = intents.recv() {
            if matches!(&*delivery, SessionIntent::Quit) {
                break;
            }
        }
        Ok(())
    });
    let face = round_face((actions, events, worker), "s1").unwrap();
    publish.send(()).unwrap();
    let root = plakous_host_protocol::views::transcript::TranscriptScope::Root;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !face.events.transcript_resnapshot_required_for(&root) {
        assert!(Instant::now() < deadline, "no row was read as lost");
        std::thread::sleep(Duration::from_millis(2));
    }
    let RoundFace {
        actions,
        events,
        mailbox,
        join,
        ..
    } = face;
    drop((actions, events, mailbox));
    joined(join);
}
