#![allow(clippy::unwrap_used, reason = "private durable receipt fixtures")]
use super::*;
use plakous_runner_protocol::retention::{Id, Job, Status};
use std::os::unix::fs::DirBuilderExt as _;

fn id(value: &str) -> Id {
    Id::try_from(value.to_owned()).unwrap()
}

fn fixture() -> (tempfile::TempDir, Registry, Bind, Line, Owner) {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("r/builds/v8-receiver");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let registry = Registry::open(&directory).unwrap();
    let scope = Bind {
        version: 1,
        host_id: id("home"),
        host_instance: id("receiver"),
    };
    let owner = Owner {
        line: id("line"),
        generation: 1,
        branch: id("branch"),
        fence: 1,
    };
    let line = Line {
        owner: owner.clone(),
        origin: Origin {
            build: id("v8-original"),
            version: 1,
            instance: id("original"),
        },
        jobs: vec![Job {
            session: id("allocation"),
            number: 1,
            command: "fixture".into(),
            started_at_ms: 0,
            attribution: "fixture".into(),
            elapsed_ms: 0,
            status: Status::Running {},
            terminal: false,
            output: String::new(),
            owner_generation: 1,
        }],
        next_number: 2,
        lost: false,
        native_disabled: true,
    };
    let to = Owner {
        fence: 2,
        ..owner.clone()
    };
    registry
        .intent(&scope, owner, to.clone(), line.clone())
        .unwrap();
    registry
        .update(&scope, &line.origin, &to, line.jobs.clone(), 2)
        .unwrap();
    (root, registry, scope, line, to)
}

#[test]
fn retention_review_intent_cannot_leapfrog_a_different_destination_tuple() {
    let (_root, registry, scope, line, to) = fixture();
    let different = Owner {
        fence: 3,
        ..to.clone()
    };
    assert!(
        registry
            .intent(&scope, line.owner.clone(), different, line)
            .is_err()
    );
    assert_eq!(registry.entries("home", None)[0].to, to);
}

#[test]
fn retention_review_late_snapshots_cannot_revive_consumed_or_lost_unreachable_jobs() {
    let (_root, registry, scope, line, to) = fixture();
    assert!(!registry.builds.join(line.origin.build.as_str()).exists());
    registry
        .acknowledged(&scope, &line.origin, &to, &[line.jobs[0].session.clone()])
        .unwrap();
    let reopened = Registry::open(registry.path.parent().unwrap()).unwrap();
    reopened
        .update(&scope, &line.origin, &to, line.jobs.clone(), 2)
        .unwrap();
    assert_eq!(reopened.entries("home", None)[0].inventory.jobs, []);
    // A different original has a separate loss fence, even without a reachable socket.
    let mut other = line.clone();
    other.origin.instance = id("another-original");
    reopened
        .intent(&scope, other.owner.clone(), to.clone(), other.clone())
        .unwrap();
    reopened.lose(&scope, &other.origin, &to).unwrap();
    reopened
        .update(&scope, &other.origin, &to, other.jobs, 2)
        .unwrap();
    let lost = reopened
        .entries("home", None)
        .into_iter()
        .find(|entry| entry.inventory.origin == other.origin)
        .unwrap();
    assert!(lost.inventory.lost);
    assert!(lost.inventory.jobs[0].terminal);
    assert_eq!(lost.inventory.jobs[0].status, Status::OutcomeUnknown {});
}

#[test]
fn retention_review_occupancy_respects_forwarding_filter_and_terminal_proof() {
    let (_root, registry, scope, mut line, to) = fixture();
    assert_eq!(registry.occupancy(|_, _| true).values().sum::<usize>(), 1);
    assert_eq!(registry.occupancy(|_, _| false).values().sum::<usize>(), 0);
    assert_eq!(
        registry
            .occupancy(|host, name| host != "home" || name != "line")
            .values()
            .sum::<usize>(),
        0
    );
    line.jobs[0].terminal = true;
    registry
        .update(&scope, &line.origin, &to, line.jobs, 2)
        .unwrap();
    assert_eq!(registry.occupancy(|_, _| true).values().sum::<usize>(), 0);
}

#[test]
fn retention_review_control_ack_targets_only_live_nonempty_relays_for_that_id() {
    let (_root, registry, scope, line, to) = fixture();
    for index in 0..4 {
        let mut other = line.clone();
        other.origin.instance = id(&format!("original-{index}"));
        registry
            .intent(&scope, other.owner.clone(), to.clone(), other.clone())
            .unwrap();
        if index != 3 {
            registry
                .control_receipt(
                    &scope,
                    &other.origin,
                    &to,
                    &id("relayed"),
                    ControlReceipt::Sent,
                )
                .unwrap();
        }
        if index == 1 {
            registry.lose(&scope, &other.origin, &to).unwrap();
        }
        if index == 2 {
            registry
                .update(&scope, &other.origin, &to, Vec::new(), 2)
                .unwrap();
        }
    }
    let targets = registry.control_targets("home", "receiver", "relayed");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].inventory.origin.instance, id("original-0"));
    registry
        .control_receipt(
            &scope,
            &targets[0].inventory.origin,
            &to,
            &id("relayed"),
            ControlReceipt::Acknowledged,
        )
        .unwrap();
    let reopened = Registry::open(registry.path.parent().unwrap()).unwrap();
    assert_eq!(
        reopened
            .control_targets("home", "receiver", "relayed")
            .len(),
        0
    );
    assert_eq!(
        registry
            .control_targets("home", "receiver", "not-relayed")
            .len(),
        0
    );
    assert_eq!(
        registry.control_targets("home", "foreign", "relayed").len(),
        0
    );
    assert_eq!(
        registry
            .control_targets("foreign", "receiver", "relayed")
            .len(),
        0
    );
}
