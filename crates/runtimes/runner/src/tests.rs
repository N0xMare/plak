use std::time::{Duration, Instant};

use plakous_runner_protocol::{
    LineOwner, Operation, Refusal, Request, RequestClass, RunnerFrame, verbs,
};
use serde_json::json;

use crate::state::{Admission, CLOCK_MARGIN, RETENTION, Tables};

const HOST: &str = "host-a";
const INSTANCE: &str = "instance-a";

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
        age_ms: None,
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
    let from = tables.owner(HOST, &owner.line).map(|(current, _)| current);
    assert_eq!(
        tables.transfer(HOST, INSTANCE, "t".into(), from.as_ref(), owner.clone()),
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
    let Admission::Run(Some(key)) =
        tables.admit(HOST, INSTANCE, &effect("r1", &head, "d", false), now)
    else {
        panic!("the first send runs");
    };
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r2", &head, "d", true), now),
        Admission::Answer(RunnerFrame::InProgress {
            request_id: "r2".into()
        })
    );
    // The answer goes to the request id the host waits under now.
    assert_eq!(
        tables.answer(HOST, &key, result("r1"), now),
        (result("r2"), Some(INSTANCE.to_owned()))
    );
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r3", &head, "d", true), now),
        Admission::Answer(result("r3"))
    );
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r4", &head, "other", false), now),
        Admission::Answer(RunnerFrame::StaleRequest {
            request_id: "r4".into()
        })
    );
    tables.acknowledge(HOST, INSTANCE, "r3");
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r5", &head, "d", true), now),
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
    let Admission::Run(Some(key)) =
        tables.admit(HOST, INSTANCE, &effect("r1", &head, "d", false), now)
    else {
        panic!("the first send runs");
    };
    transferred(&mut tables, &owner(1, 2));
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r2", &head, "d", true), now),
        Admission::Answer(RunnerFrame::InProgress {
            request_id: "r2".into()
        })
    );
    tables.answer(HOST, &key, result("r1"), now);
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r3", &head, "d", true), now),
        Admission::Answer(result("r3"))
    );
    // A new key of the old owner is fenced.
    let mut other = effect("r4", &head, "d", false);
    other.class = RequestClass::EffectCall {
        call_id: "call-2".into(),
        attempt_id: "attempt-1".into(),
    };
    assert!(matches!(
        tables.admit(HOST, INSTANCE, &other, now),
        Admission::Answer(RunnerFrame::StaleOwner { .. })
    ));
    assert_eq!(tables.key_for(HOST, INSTANCE, "r3"), Some(key));
}

#[test]
fn a_replayed_unseen_key_is_tombstoned_so_a_late_original_never_runs() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r1", &head, "d", true), now),
        Admission::Answer(RunnerFrame::NotSeen {
            request_id: "r1".into()
        })
    );
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r1", &head, "d", false), now),
        Admission::Answer(RunnerFrame::NotSeen {
            request_id: "r1".into()
        })
    );
    // Past retention the tombstone is gone, and the key is new again.
    assert!(matches!(
        tables.admit(
            HOST,
            INSTANCE,
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
        tables.admit(HOST, INSTANCE, &effect("r1", &first, "d", false), now),
        Admission::Answer(RunnerFrame::Refused {
            request_id: Some("r1".into()),
            refusal: Refusal::OwnerNotTransferred,
        })
    );
    transferred(&mut tables, &first);
    let successor = owner(2, 1);
    transferred(&mut tables, &successor);
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r2", &first, "d", false), now),
        Admission::Answer(RunnerFrame::StaleOwner {
            request_id: Some("r2".into()),
            control_id: None,
            current: successor.clone(),
        })
    );
    assert_eq!(
        tables.transfer(HOST, INSTANCE, "t2".into(), Some(&successor), first),
        RunnerFrame::StaleOwner {
            request_id: None,
            control_id: Some("t2".into()),
            current: successor.clone(),
        }
    );
    // The accepted owner again is a retry of a lost acknowledgement.
    assert_eq!(
        tables.transfer(HOST, INSTANCE, "t3".into(), None, successor.clone()),
        RunnerFrame::TransferAccepted {
            control_id: "t3".into()
        }
    );
    transferred(&mut tables, &owner(2, 2));
    // A newer owner that was never transferred is refused, not adopted.
    assert_eq!(
        tables.admit(HOST, INSTANCE, &effect("r3", &owner(3, 1), "d", false), now),
        Admission::Answer(RunnerFrame::Refused {
            request_id: Some("r3".into()),
            refusal: Refusal::OwnerNotTransferred,
        })
    );
    // Each host fences its own lines.
    assert_eq!(
        tables.transfer("host-b", INSTANCE, "t4".into(), None, owner(1, 1)),
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
        tables.admit(HOST, INSTANCE, &ownerless, now),
        Admission::Answer(RunnerFrame::Refused {
            refusal: Refusal::Malformed { .. },
            ..
        })
    ));
    let mut read = ownerless;
    read.class = RequestClass::BuildRead;
    read.replay = true;
    assert_eq!(
        tables.admit(HOST, INSTANCE, &read, now),
        Admission::Run(None)
    );
    assert_eq!(
        tables.admit(HOST, INSTANCE, &read, now),
        Admission::Run(None)
    );
}

#[test]
fn an_owner_control_runs_once_and_an_observation_is_fenced_but_never_recorded() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    let mut observation = effect("r1", &head, "d", false);
    observation.class = RequestClass::ProcessObservation { job: 1, cursor: 0 };
    assert_eq!(
        tables.admit(HOST, INSTANCE, &observation, now),
        Admission::Answer(RunnerFrame::Refused {
            request_id: Some("r1".into()),
            refusal: Refusal::OwnerNotTransferred,
        })
    );
    transferred(&mut tables, &head);
    assert_eq!(
        tables.admit(HOST, INSTANCE, &observation, now),
        Admission::Run(None)
    );
    assert_eq!(
        tables.admit(HOST, INSTANCE, &observation, now),
        Admission::Run(None)
    );

    let mut control = effect("r2", &head, "d", false);
    control.class = RequestClass::OwnerControl {
        control_id: "c1".into(),
    };
    assert!(matches!(
        tables.admit(HOST, INSTANCE, &control, now),
        Admission::Run(Some(_))
    ));
    control.request_id = "r3".into();
    control.replay = true;
    assert_eq!(
        tables.admit(HOST, INSTANCE, &control, now),
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
        tables.admit(HOST, INSTANCE, &start, now),
        Admission::Run(Some(_))
    ));
    let mut write = effect("r2", &head, "d2", false);
    write.operation.verb = verbs::PROCESS_WRITE.into();
    assert!(matches!(
        tables.admit(HOST, INSTANCE, &write, now),
        Admission::Run(Some(_))
    ));
    write.request_id = "r3".into();
    assert_eq!(
        tables.admit(HOST, INSTANCE, &write, now),
        Admission::Answer(RunnerFrame::InProgress {
            request_id: "r3".into()
        })
    );
}

#[test]
fn a_transfer_to_a_newer_owner_is_a_compare_and_swap_from_the_accepted_one() {
    let mut tables = Tables::default();
    let first = owner(1, 1);
    // A line with no owner takes any first owner.
    assert_eq!(
        tables.transfer(
            HOST,
            INSTANCE,
            "t1".into(),
            Some(&owner(0, 9)),
            first.clone()
        ),
        RunnerFrame::TransferAccepted {
            control_id: "t1".into()
        }
    );
    let successor = owner(1, 2);
    // From an owner the runner does not accept: refused, naming the one it
    // does.
    assert_eq!(
        tables.transfer(HOST, INSTANCE, "t2".into(), None, successor.clone()),
        RunnerFrame::StaleOwner {
            request_id: None,
            control_id: Some("t2".into()),
            current: first.clone(),
        }
    );
    assert_eq!(
        tables.transfer(HOST, INSTANCE, "t3".into(), Some(&first), successor.clone()),
        RunnerFrame::TransferAccepted {
            control_id: "t3".into()
        }
    );
    // The accepted owner again is a retry, whatever `from` says.
    assert_eq!(
        tables.transfer(HOST, INSTANCE, "t4".into(), Some(&first), successor.clone()),
        RunnerFrame::TransferAccepted {
            control_id: "t4".into()
        }
    );
    let mut other_line = owner(5, 5);
    other_line.line = "line-2".into();
    assert!(matches!(
        tables.transfer(HOST, INSTANCE, "t5".into(), Some(&successor), other_line),
        RunnerFrame::TransferRefused { .. }
    ));
}

#[test]
fn a_replay_past_retention_is_expired_not_proven_unseen() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    let mut late = effect("r1", &head, "d", true);
    late.age_ms =
        Some(u64::try_from(RETENTION.checked_sub(CLOCK_MARGIN).unwrap().as_millis()).unwrap());
    assert_eq!(
        tables.admit(HOST, INSTANCE, &late, now),
        Admission::Answer(RunnerFrame::Expired {
            request_id: "r1".into()
        })
    );
    late.age_ms = Some(1_000);
    assert_eq!(
        tables.admit(HOST, INSTANCE, &late, now),
        Admission::Answer(RunnerFrame::NotSeen {
            request_id: "r1".into()
        })
    );
}

#[test]
fn a_keys_answer_and_acknowledgement_belong_to_the_instance_that_last_sent_it() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    let Admission::Run(Some(key)) =
        tables.admit(HOST, INSTANCE, &effect("r1", &head, "d", false), now)
    else {
        panic!("the first send runs");
    };
    // The host restarted: its new instance takes the line with a newer
    // owner, then replays the key its predecessor sent.
    assert_eq!(
        tables.transfer(HOST, "instance-b", "t2".into(), Some(&head), owner(1, 2)),
        RunnerFrame::TransferAccepted {
            control_id: "t2".into()
        }
    );
    tables.admit(HOST, "instance-b", &effect("s1", &head, "d", true), now);
    assert_eq!(
        tables.answer(HOST, &key, result("r1"), now),
        (result("s1"), Some("instance-b".to_owned()))
    );
    // The old instance's request ids are not the new one's.
    tables.acknowledge(HOST, INSTANCE, "s1");
    assert!(tables.holds_unacknowledged(now));
    tables.acknowledge(HOST, "instance-b", "s1");
    assert!(!tables.holds_unacknowledged(now));
}

struct RefusingFloors;

impl crate::floors::OwnerFloors for RefusingFloors {
    fn load(&self) -> Vec<(String, crate::floors::Floor)> {
        Vec::new()
    }

    fn record(&self, _: &str, _: &crate::floors::Floor) -> std::io::Result<()> {
        Err(std::io::Error::other("disk full"))
    }
}

#[test]
fn owner_floors_outlive_a_restart_and_an_unrecorded_owner_is_not_accepted() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("owners.json");
    let floors = std::sync::Arc::new(crate::floors::FileOwnerFloors::open(&path).unwrap());
    let mut tables = Tables::with_floors(floors);
    let newer = owner(2, 1);
    transferred(&mut tables, &newer);
    drop(tables);

    // A restart reads the floor back: the older owner is refused, and the
    // recorded one is accepted but no longer held until re-adopted.
    let floors = std::sync::Arc::new(crate::floors::FileOwnerFloors::open(&path).unwrap());
    let mut restarted = Tables::with_floors(floors);
    assert_eq!(
        restarted.owner(HOST, &newer.line),
        Some((newer.clone(), false))
    );
    assert!(matches!(
        restarted.admit(
            HOST,
            INSTANCE,
            &effect("r1", &owner(1, 9), "d", false),
            Instant::now()
        ),
        Admission::Answer(RunnerFrame::StaleOwner { .. })
    ));
    // The recorded owner itself is transferred again before a call.
    assert_eq!(
        restarted.admit(
            HOST,
            INSTANCE,
            &effect("r2", &newer, "d", false),
            Instant::now()
        ),
        Admission::Answer(RunnerFrame::Refused {
            request_id: Some("r2".into()),
            refusal: Refusal::OwnerNotTransferred,
        })
    );
    transferred(&mut restarted, &newer);
    assert!(restarted.owner(HOST, &newer.line).unwrap().1);
    assert!(matches!(
        restarted.admit(
            HOST,
            INSTANCE,
            &effect("r3", &newer, "d", false),
            Instant::now()
        ),
        Admission::Run(_)
    ));
    transferred(&mut restarted, &owner(2, 2));
    assert!(restarted.owner(HOST, &newer.line).unwrap().1);

    let mut refusing = Tables::with_floors(std::sync::Arc::new(RefusingFloors));
    assert!(matches!(
        refusing.transfer(HOST, INSTANCE, "t".into(), None, owner(1, 1)),
        RunnerFrame::TransferRefused { .. }
    ));
    assert_eq!(refusing.owner(HOST, "line-1"), None);
}

const SIBLING: &str = "instance-b";

fn control(id: &str, owner: &LineOwner) -> Request {
    Request {
        class: RequestClass::OwnerControl {
            control_id: format!("control-{id}"),
        },
        operation: Operation {
            verb: verbs::JOBS_STOP.into(),
            payload: json!({}),
        },
        ..effect(id, owner, "d", false)
    }
}

fn transfer_from(
    tables: &mut Tables,
    instance: &str,
    from: Option<&LineOwner>,
    to: &LineOwner,
) -> RunnerFrame {
    tables.transfer(HOST, instance, "t".into(), from, to.clone())
}

fn accepted() -> RunnerFrame {
    RunnerFrame::TransferAccepted {
        control_id: "t".into(),
    }
}

fn stale(current: &LineOwner) -> RunnerFrame {
    RunnerFrame::StaleOwner {
        request_id: None,
        control_id: Some("t".into()),
        current: current.clone(),
    }
}

fn not_transferred(id: &str) -> Admission {
    Admission::Answer(RunnerFrame::Refused {
        request_id: Some(id.into()),
        refusal: Refusal::OwnerNotTransferred,
    })
}

#[test]
fn a_sibling_cannot_signal_or_call_on_a_held_line() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    assert_eq!(
        tables.admit(HOST, SIBLING, &effect("s1", &head, "d", false), now),
        not_transferred("s1")
    );
    assert_eq!(
        tables.admit(HOST, SIBLING, &control("s2", &head), now),
        not_transferred("s2")
    );
    assert!(matches!(
        tables.admit(HOST, INSTANCE, &control("h1", &head), now),
        Admission::Run(Some(_))
    ));
}

/// A sibling cannot take the holder's keys: not its answer, its result or
/// its acknowledgement, and it cannot tombstone a key the holder is about to
/// send.
#[test]
fn a_sibling_cannot_readdress_or_tombstone_the_holder_keys() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    let Admission::Run(Some(key)) =
        tables.admit(HOST, INSTANCE, &effect("r1", &head, "d", false), now)
    else {
        panic!("the holder's send runs");
    };
    // A sibling's resend and replay of the running key are refused.
    assert_eq!(
        tables.admit(HOST, SIBLING, &effect("s1", &head, "d", false), now),
        not_transferred("s1")
    );
    assert_eq!(
        tables.admit(HOST, SIBLING, &effect("s2", &head, "d", true), now),
        not_transferred("s2")
    );
    // The answer still goes to the holder, and only its Ack spends the key.
    assert_eq!(
        tables.answer(HOST, &key, result("r1"), now),
        (result("r1"), Some(INSTANCE.to_owned()))
    );
    assert_eq!(
        tables.admit(HOST, SIBLING, &effect("s3", &head, "d", true), now),
        not_transferred("s3")
    );
    tables.acknowledge(HOST, SIBLING, "r1");
    assert!(tables.holds_unacknowledged(now));
    tables.acknowledge(HOST, INSTANCE, "r1");
    assert!(!tables.holds_unacknowledged(now));
    // A sibling's replay of an unseen key tombstones nothing: the holder's
    // first send of that key runs.
    let unseen = Request {
        class: RequestClass::EffectCall {
            call_id: "call-2".into(),
            attempt_id: "attempt-1".into(),
        },
        ..effect("s4", &head, "d", true)
    };
    assert_eq!(
        tables.admit(HOST, SIBLING, &unseen, now),
        not_transferred("s4")
    );
    let first = Request {
        request_id: "r2".into(),
        replay: false,
        ..unseen
    };
    assert!(matches!(
        tables.admit(HOST, INSTANCE, &first, now),
        Admission::Run(Some(_))
    ));
}

#[test]
fn only_the_holder_transfers_the_same_owner_again() {
    let mut tables = Tables::default();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    tables.orphan(HOST, INSTANCE);
    assert_eq!(tables.owner(HOST, &head.line), Some((head.clone(), false)));
    // Orphaned is not free to take: a sibling with the same owner is stale.
    assert_eq!(
        transfer_from(&mut tables, SIBLING, Some(&head), &head),
        stale(&head)
    );
    assert_eq!(tables.owner(HOST, &head.line), Some((head.clone(), false)));
    // The holder returning re-adopts it.
    assert_eq!(
        transfer_from(&mut tables, INSTANCE, None, &head),
        accepted()
    );
    assert_eq!(tables.owner(HOST, &head.line), Some((head, true)));
}

#[test]
fn a_sibling_takes_a_line_only_with_a_newer_owner() {
    let mut tables = Tables::default();
    let now = Instant::now();
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    let newer = owner(1, 2);
    assert_eq!(
        transfer_from(&mut tables, SIBLING, Some(&head), &newer),
        accepted()
    );
    assert!(matches!(
        tables.admit(HOST, INSTANCE, &effect("h1", &head, "d", false), now),
        Admission::Answer(RunnerFrame::StaleOwner { .. })
    ));
    assert_eq!(
        tables.admit(HOST, INSTANCE, &control("h2", &newer), now),
        not_transferred("h2")
    );
    assert!(matches!(
        tables.admit(HOST, SIBLING, &control("s1", &newer), now),
        Admission::Run(Some(_))
    ));
}

#[test]
fn an_expired_lease_orphans_only_its_instance_lines() {
    let mut tables = Tables::default();
    let held = owner(1, 1);
    let sibling_line = LineOwner {
        line: "line-2".into(),
        ..owner(1, 1)
    };
    transferred(&mut tables, &held);
    assert_eq!(
        transfer_from(&mut tables, SIBLING, None, &sibling_line),
        accepted()
    );
    tables.orphan(HOST, SIBLING);
    assert_eq!(tables.owner(HOST, "line-1"), Some((held, true)));
    assert_eq!(tables.owner(HOST, "line-2"), Some((sibling_line, false)));
}

/// A transfer the runner floored and then lost the acknowledgement of, in a
/// restart, is retried by its holder and accepted; a sibling racing the
/// restart with the floor owner is refused and never becomes the holder.
#[test]
fn after_a_restart_the_floor_owner_is_accepted_only_from_its_holder() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("owners.json");
    let floors = std::sync::Arc::new(crate::floors::FileOwnerFloors::open(&path).unwrap());
    let mut tables = Tables::with_floors(floors);
    let head = owner(1, 1);
    transferred(&mut tables, &head);
    drop(tables);

    let floors = std::sync::Arc::new(crate::floors::FileOwnerFloors::open(&path).unwrap());
    let mut restarted = Tables::with_floors(floors);
    assert_eq!(
        transfer_from(&mut restarted, SIBLING, None, &head),
        stale(&head)
    );
    assert_eq!(
        restarted.admit(
            HOST,
            SIBLING,
            &effect("s1", &head, "d", false),
            Instant::now()
        ),
        not_transferred("s1")
    );
    assert_eq!(
        transfer_from(&mut restarted, INSTANCE, None, &head),
        accepted()
    );
    assert_eq!(
        restarted.owner(HOST, &head.line),
        Some((head.clone(), true))
    );
    // A newer owner, which only the line's lease holder can mint, is
    // accepted from anyone and becomes the floor with its holder.
    let newer = owner(1, 2);
    assert_eq!(
        transfer_from(&mut restarted, SIBLING, Some(&head), &newer),
        accepted()
    );
    drop(restarted);
    let floors = std::sync::Arc::new(crate::floors::FileOwnerFloors::open(&path).unwrap());
    let mut again = Tables::with_floors(floors);
    assert_eq!(
        transfer_from(&mut again, INSTANCE, None, &newer),
        stale(&newer)
    );
    assert_eq!(transfer_from(&mut again, SIBLING, None, &newer), accepted());
}

#[test]
fn a_floors_file_of_an_earlier_format_is_refused_by_name() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("owners.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({ "host-a": { "line-1": owner(1, 1) } })).unwrap(),
    )
    .unwrap();
    let Err(error) = crate::floors::FileOwnerFloors::open(&path) else {
        panic!("an earlier format must not load");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let message = error.to_string();
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(
        message.contains("stop every host of the paired homes, then remove"),
        "{message}"
    );
}
