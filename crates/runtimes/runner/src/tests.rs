use std::time::{Duration, Instant};

use plakous_runner_protocol::{
    LineOwner, Operation, Refusal, Request, RequestClass, RunnerFrame, verbs,
};
use serde_json::json;

use crate::state::{Admission, RETENTION, Tables};

const HOST: &str = "host-a";

fn owner(generation: u64, fence: u64) -> LineOwner {
    LineOwner {
        line: "line-1".into(),
        generation,
        branch: "branch-1".into(),
        fence,
    }
}

fn effect(id: &str, owner: &LineOwner, digest: &str, replay: bool) -> Request {
    Request {
        request_id: id.into(),
        class: RequestClass::EffectCall {
            call_id: "call-1".into(),
            attempt_id: "attempt-1".into(),
        },
        owner: Some(owner.clone()),
        deadline_ms: None,
        digest: digest.into(),
        replay,
        operation: Operation {
            verb: verbs::MUTATE_WORKSPACE.into(),
            payload: json!({}),
        },
    }
}

fn result(id: &str) -> RunnerFrame {
    RunnerFrame::Result {
        request_id: id.into(),
        payload: json!({"ran": true}),
    }
}

fn transferred(tables: &mut Tables, owner: &LineOwner) {
    assert_eq!(
        tables.transfer(HOST, "t".into(), owner.clone()),
        RunnerFrame::TransferAccepted {
            control_id: "t".into()
        }
    );
}

#[test]
fn an_effect_call_key_runs_once_and_answers_every_resend() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    let Admission::Run(Some(key)) = tables.admit(HOST, &effect("r1", &head, "d", false), now)
    else {
        panic!("the first send runs");
    };
    assert_eq!(
        tables.admit(HOST, &effect("r2", &head, "d", true), now),
        Admission::Answer(RunnerFrame::InProgress {
            request_id: "r2".into()
        })
    );
    // The answer goes to the request id the host waits under now.
    assert_eq!(tables.answer(HOST, &key, result("r1"), now), result("r2"));
    assert_eq!(
        tables.admit(HOST, &effect("r3", &head, "d", true), now),
        Admission::Answer(result("r3"))
    );
    assert_eq!(
        tables.admit(HOST, &effect("r4", &head, "other", false), now),
        Admission::Answer(RunnerFrame::StaleRequest {
            request_id: "r4".into()
        })
    );
    tables.acknowledge(HOST, "r3");
    assert_eq!(
        tables.admit(HOST, &effect("r5", &head, "d", true), now),
        Admission::Answer(RunnerFrame::Spent {
            request_id: "r5".into()
        })
    );
}

#[test]
fn a_key_that_ran_is_answered_after_an_owner_change_not_fenced() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    let Admission::Run(Some(key)) = tables.admit(HOST, &effect("r1", &head, "d", false), now)
    else {
        panic!("the first send runs");
    };
    transferred(&mut tables, &owner(1, 2));
    assert_eq!(
        tables.admit(HOST, &effect("r2", &head, "d", true), now),
        Admission::Answer(RunnerFrame::InProgress {
            request_id: "r2".into()
        })
    );
    tables.answer(HOST, &key, result("r1"), now);
    assert_eq!(
        tables.admit(HOST, &effect("r3", &head, "d", true), now),
        Admission::Answer(result("r3"))
    );
    // A new key of the old owner is fenced.
    let mut other = effect("r4", &head, "d", false);
    other.class = RequestClass::EffectCall {
        call_id: "call-2".into(),
        attempt_id: "attempt-1".into(),
    };
    assert!(matches!(
        tables.admit(HOST, &other, now),
        Admission::Answer(RunnerFrame::StaleOwner { .. })
    ));
    assert_eq!(tables.key_for(HOST, "r3"), Some(key));
}

#[test]
fn a_replayed_unseen_key_is_tombstoned_so_a_late_original_never_runs() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    assert_eq!(
        tables.admit(HOST, &effect("r1", &head, "d", true), now),
        Admission::Answer(RunnerFrame::NotSeen {
            request_id: "r1".into()
        })
    );
    assert_eq!(
        tables.admit(HOST, &effect("r1", &head, "d", false), now),
        Admission::Answer(RunnerFrame::NotSeen {
            request_id: "r1".into()
        })
    );
    // Past retention the tombstone is gone, and the key is new again.
    assert!(matches!(
        tables.admit(
            HOST,
            &effect("r2", &head, "d", false),
            now + RETENTION + Duration::from_secs(1)
        ),
        Admission::Run(Some(_))
    ));
}

#[test]
fn owners_are_ordered_by_generation_then_fence_and_must_be_transferred() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let first = owner(1, 5);
    assert_eq!(
        tables.admit(HOST, &effect("r1", &first, "d", false), now),
        Admission::Answer(RunnerFrame::Refused {
            request_id: Some("r1".into()),
            refusal: Refusal::OwnerNotTransferred,
        })
    );
    transferred(&mut tables, &first);
    let successor = owner(2, 1);
    transferred(&mut tables, &successor);
    assert_eq!(
        tables.admit(HOST, &effect("r2", &first, "d", false), now),
        Admission::Answer(RunnerFrame::StaleOwner {
            request_id: Some("r2".into()),
            control_id: None,
            current: successor.clone(),
        })
    );
    assert_eq!(
        tables.transfer(HOST, "t2".into(), first),
        RunnerFrame::StaleOwner {
            request_id: None,
            control_id: Some("t2".into()),
            current: successor.clone(),
        }
    );
    // The accepted owner again is a retry of a lost acknowledgement.
    assert_eq!(
        tables.transfer(HOST, "t3".into(), successor.clone()),
        RunnerFrame::TransferAccepted {
            control_id: "t3".into()
        }
    );
    transferred(&mut tables, &owner(2, 2));
    // A newer owner that was never transferred is refused, not adopted.
    assert_eq!(
        tables.admit(HOST, &effect("r3", &owner(3, 1), "d", false), now),
        Admission::Answer(RunnerFrame::Refused {
            request_id: Some("r3".into()),
            refusal: Refusal::OwnerNotTransferred,
        })
    );
    // Each host fences its own lines.
    assert_eq!(
        tables.transfer("host-b", "t4".into(), owner(1, 1)),
        RunnerFrame::TransferAccepted {
            control_id: "t4".into()
        }
    );
}

#[test]
fn an_effect_call_names_its_owner_and_a_build_read_is_never_recorded() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let mut ownerless = effect("r1", &owner(1, 1), "d", false);
    ownerless.owner = None;
    assert!(matches!(
        tables.admit(HOST, &ownerless, now),
        Admission::Answer(RunnerFrame::Refused {
            refusal: Refusal::Malformed { .. },
            ..
        })
    ));
    let mut read = ownerless;
    read.class = RequestClass::BuildRead;
    read.replay = true;
    assert_eq!(tables.admit(HOST, &read, now), Admission::Run(None));
    assert_eq!(tables.admit(HOST, &read, now), Admission::Run(None));
}

#[test]
fn an_owner_control_runs_once_and_an_observation_is_fenced_but_never_recorded() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    let mut observation = effect("r1", &head, "d", false);
    observation.class = RequestClass::ProcessObservation { job: 1, cursor: 0 };
    assert_eq!(
        tables.admit(HOST, &observation, now),
        Admission::Answer(RunnerFrame::Refused {
            request_id: Some("r1".into()),
            refusal: Refusal::OwnerNotTransferred,
        })
    );
    transferred(&mut tables, &head);
    assert_eq!(tables.admit(HOST, &observation, now), Admission::Run(None));
    assert_eq!(tables.admit(HOST, &observation, now), Admission::Run(None));

    let mut control = effect("r2", &head, "d", false);
    control.class = RequestClass::OwnerControl {
        control_id: "c1".into(),
    };
    assert!(matches!(
        tables.admit(HOST, &control, now),
        Admission::Run(Some(_))
    ));
    control.request_id = "r3".into();
    control.replay = true;
    assert_eq!(
        tables.admit(HOST, &control, now),
        Admission::Answer(RunnerFrame::InProgress {
            request_id: "r3".into()
        })
    );
}

#[test]
fn one_call_may_run_several_verbs_each_once() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    let start = effect("r1", &head, "d1", false);
    assert!(matches!(
        tables.admit(HOST, &start, now),
        Admission::Run(Some(_))
    ));
    let mut write = effect("r2", &head, "d2", false);
    write.operation.verb = verbs::PROCESS_WRITE.into();
    assert!(matches!(
        tables.admit(HOST, &write, now),
        Admission::Run(Some(_))
    ));
    write.request_id = "r3".into();
    assert_eq!(
        tables.admit(HOST, &write, now),
        Admission::Answer(RunnerFrame::InProgress {
            request_id: "r3".into()
        })
    );
}
