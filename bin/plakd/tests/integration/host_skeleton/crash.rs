//! Crash windows after creation (spec 05 §2.6, “Host loss, release and handoff”):
//! kill the host at every indexed commit of start-scan open and continuation,
//! of a `release_session` waiting on a running turn, and of a `release_session`
//! for a root whose tree is in custody. The next host recovers each cut.
//!
//! A durable handoff can authorize one continuation. A cut before that
//! decision leaves the turn held; another restart must not continue it again.
//! A busy release is not a handoff and must not make an answered turn look
//! lost. Tree release is witnessed by the next attachment taking custody,
//! independently of its runtime lease.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use plakous_offline_crash_harness::commit_trace::{KILL_ENV, TRACE_ENV};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};

use super::*;

pub(super) mod client_control;
mod compaction_queue;
mod restart_matrix;
mod wire;

/// The physical provider dispatches the fixture has seen.
pub(super) fn dispatches(fixture: &Fixture) -> usize {
    read_evidence(fixture.root.join("evidence.jsonl"))
        .expect("dispatch evidence")
        .iter()
        .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
        .count()
}

fn trace_env(trace: &Path) -> (&'static str, String) {
    (TRACE_ENV, trace.display().to_string())
}

/// The environment of a host traced to `trace` and killed at `store`'s
/// commit `ordinal`.
fn cut_env(trace: &Path, store: &str, ordinal: u64) -> [(&'static str, String); 2] {
    [trace_env(trace), (KILL_ENV, format!("{store}:{ordinal}"))]
}

/// Asks `host` to stop and waits for it, however it ends: a cut may kill it
/// during its stop.
fn stop_or_cut(host: &mut Host) {
    let Some(mut child) = host.child.take() else {
        return;
    };
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let deadline = Instant::now() + WAIT;
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "plakd host did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `host` was killed: its process ended, and not cleanly.
fn was_cut(host: &mut Host) -> bool {
    match host.child.as_mut().unwrap().try_wait().unwrap() {
        Some(status) => {
            assert!(!status.success(), "the host ended cleanly");
            host.child = None;
            true
        }
        None => false,
    }
}

const STALLED: &str = "a turn the stop hands off";

/// A session whose running root turn a `SIGTERM` handed off, and the
/// dispatches made so far; the next turn answers [`CONTINUED`].
fn handed_off(fixture: &Fixture) -> (String, NodeAddress, usize) {
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"stream": [{"text": PREFIX}, {"stall": 30000}]}]
    }));
    let mut host = start(fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, STALLED, PREFIX);
    drop(client);
    stop(&mut host);
    assert_eq!(
        root_record(fixture, &session).0,
        [RootInterruptCause::Handoff]
    );
    fixture.write_script(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": CONTINUED}}]
    }));
    (session.clone(), node, dispatches(fixture))
}

/// Whether the handed-off root was continued: one decision, and its turn
/// dispatched once.
fn continued(fixture: &Fixture, session: &str, before: usize) -> bool {
    root_record(fixture, session).1 == 1 && dispatches(fixture) > before
}

/// Cut the successor after a planned handoff at every indexed commit of its
/// start-scan open and its continuation. A third host settles the root:
/// the continuation runs, or its turn is held as a loss, never with two
/// decisions or a second dispatch, and a fourth host continues nothing.
#[test]
fn host_crash_cuts_of_the_start_scan_continue_once_or_hold() {
    let Some(probe) = fixture() else { return };
    drop(probe);
    let baseline = fixture().unwrap();
    let (session, _, before) = handed_off(&baseline);
    let trace = baseline.root.join("scan-trace.jsonl");
    let mut host = start_env(&baseline, &[], &[trace_env(&trace)]);
    let deadline = Instant::now() + WAIT;
    while !continued(&baseline, &session, before) {
        assert!(Instant::now() < deadline, "the baseline never continued");
        std::thread::sleep(Duration::from_millis(50));
    }
    stop(&mut host);
    let commits = indexed_commits(&trace);
    assert!(!commits.is_empty(), "the scan committed nothing");
    // Match the quarter-pool reservation. Each cut owns its entire fixture;
    // only the immutable list from the baseline scan is shared.
    let jobs = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(16)
        .div_ceil(4);
    let outcomes = std::thread::scope(|scope| {
        let workers: Vec<_> = commits
            .chunks(commits.len().div_ceil(jobs))
            .map(|chunk| {
                scope.spawn(|| {
                    chunk
                        .iter()
                        .map(|(_, store, ordinal)| check_start_scan_cut(store, *ordinal))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut outcomes = std::collections::BTreeMap::<&str, usize>::new();
        for worker in workers {
            for outcome in worker.join().unwrap() {
                *outcomes.entry(outcome).or_default() += 1;
            }
        }
        outcomes
    });
    eprintln!("start scan cuts: {outcomes:?}");
}

fn check_start_scan_cut(store: &str, store_ordinal: u64) -> &'static str {
    let cut = format!("cut {store}:{store_ordinal}");
    let fixture = fixture().unwrap();
    let (session, node, before) = handed_off(&fixture);
    let cut_trace = fixture.root.join("cut-trace.jsonl");
    if let Some(mut host) = try_start(&fixture, &[], &cut_env(&cut_trace, store, store_ordinal)) {
        let deadline = Instant::now() + WAIT;
        while !was_cut(&mut host) {
            if continued(&fixture, &session, before) {
                // The cut falls in the stop, if anywhere.
                stop_or_cut(&mut host);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{cut}: neither cut nor continued"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    let deadline = Instant::now() + WAIT;
    let mut id = 2;
    let outcome = loop {
        id += 1;
        let activity = client.rows(id)[0].activity;
        if activity == SessionActivity::HeldAfterLoss {
            break "held";
        }
        if activity == SessionActivity::Idle && continued(&fixture, &session, before) {
            break "continued";
        }
        assert!(
            Instant::now() < deadline,
            "{cut}: never settled ({activity:?})"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(client);
    stop(&mut host);
    let decisions = root_record(&fixture, &session).1;
    assert!(decisions <= 1, "{cut}: {decisions} decisions");
    let dispatched = dispatches(&fixture);
    assert!(
        dispatched <= before + 1,
        "{cut}: {} continuation dispatches",
        dispatched - before
    );
    let mut host = start(&fixture);
    Client::connect(&host).rows(1);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !host
            .lines
            .try_iter()
            .any(|line| line.starts_with("plakd host: continuing session ")),
        "{cut}: a settled root was continued again"
    );
    stop(&mut host);
    assert_eq!(root_record(&fixture, &session).1, decisions, "{cut}");
    assert_eq!(dispatches(&fixture), dispatched, "{cut}");
    outcome
}

/// Whether the session's durable transcript holds the turn's answer.
fn answered(fixture: &Fixture, session: &str) -> bool {
    use plakd::store::session_records::SessionRecordStore as _;
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    plakd::store::session_records::SqliteSessionStore::new(paths)
        .load_views(session)
        .ok()
        .flatten()
        .is_some_and(|views| format!("{:?}", views.rows).contains(ANSWER))
}

/// A turn that runs a while and a `release_session` sent while it runs,
/// which `released` records answered. The trace ordinal at the release's
/// request, unless the host went first.
fn busy_release(
    fixture: &Fixture,
    host: &Host,
    trace: &Path,
    released: &AtomicBool,
) -> Option<u64> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut client = Client::connect(host);
        let Outcome::Ok(Answer::Created { session, node, .. }) =
            client.request(1, new_session(&fixture.workspace()))
        else {
            panic!("not created");
        };
        let attachment = client.attach(2, &node);
        client.prompt(3, attachment, "a turn the release waits on", PREFIX);
        let release = traced(trace);
        assert_eq!(
            client.request(4, Op::ReleaseSession { session }),
            Outcome::Ok(Answer::Released)
        );
        released.store(true, Ordering::SeqCst);
        release
    }))
    .ok()
}

fn busy_script() -> serde_json::Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{
            "stream": [{"text": PREFIX}, {"stall": 2000}],
            "output": {"text": ANSWER}
        }]
    })
}

/// A `release_session` that arrives during a root turn waits for it. Kill at every
/// indexed commit from the request on, and the next host reads the dead
/// host gone and opens the session, with no handoff, no decision, no second
/// dispatch of the turn, and an answered turn never read as lost.
#[test]
fn host_crash_cuts_of_a_busy_release_lose_no_answered_turn() {
    let Some(probe) = fixture() else { return };
    drop(probe);
    let baseline = fixture().unwrap();
    baseline.write_script(&busy_script());
    let trace = baseline.root.join("release-trace.jsonl");
    let mut host = start_env(&baseline, &[], &[trace_env(&trace)]);
    let asked = Instant::now();
    let release = busy_release(&baseline, &host, &trace, &AtomicBool::new(false))
        .expect("the baseline released");
    assert!(
        asked.elapsed() >= Duration::from_secs(1),
        "the release did not wait for the turn"
    );
    stop(&mut host);
    let commits: Vec<_> = indexed_commits(&trace)
        .into_iter()
        .filter(|(ordinal, ..)| *ordinal > release)
        .collect();
    assert!(!commits.is_empty(), "the release committed nothing");
    let mut outcomes = std::collections::BTreeMap::<&str, usize>::new();
    for (_, store, store_ordinal) in &commits {
        let cut = format!("cut {store}:{store_ordinal}");
        let fixture = fixture().unwrap();
        fixture.write_script(&busy_script());
        let cut_trace = fixture.root.join("cut-trace.jsonl");
        let released = AtomicBool::new(false);
        if let Some(mut host) =
            try_start(&fixture, &[], &cut_env(&cut_trace, store, *store_ordinal))
        {
            if busy_release(&fixture, &host, &cut_trace, &released).is_some() {
                stop_or_cut(&mut host);
            } else {
                let status = host.child.take().unwrap().wait().unwrap();
                assert!(!status.success(), "{cut}: the host ended cleanly");
            }
        }
        let before = dispatches(&fixture);
        let host = start(&fixture);
        let mut client = Client::connect(&host);
        let Outcome::Ok(Answer::Created { session, node, .. }) =
            client.request(1, new_session(&fixture.workspace()))
        else {
            panic!("{cut}: the replay made no session");
        };
        let rows = client.rows(2);
        assert_eq!(
            rows[0].hosted_elsewhere, None,
            "{cut}: the dead host reads alive"
        );
        client.attach(3, &node);
        let (interrupts, decisions) = root_record(&fixture, &session);
        assert!(
            !interrupts.contains(&RootInterruptCause::Handoff),
            "{cut}: {interrupts:?}"
        );
        assert_eq!(decisions, 0, "{cut}: nothing continues by itself");
        let answered_turn = answered(&fixture, &session);
        assert!(
            !(answered_turn && interrupts.contains(&RootInterruptCause::OwnerLost)),
            "{cut}: an answered turn was read as lost"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            dispatches(&fixture),
            before,
            "{cut}: the turn was dispatched again"
        );
        let outcome = if released.load(Ordering::SeqCst) {
            "released"
        } else if answered_turn {
            "answered"
        } else {
            "in the turn"
        };
        *outcomes.entry(outcome).or_default() += 1;
    }
    eprintln!("busy release cuts: {outcomes:?}");
}

/// Cut a `release_session` for a root whose tree is in custody at
/// every indexed commit of the release, and the next host reads the dead
/// host gone and its attach takes the tree's custody, with no handoff, no
/// decision and no lost turn.
#[test]
fn host_crash_cuts_of_a_custody_release_hand_the_tree_to_the_next_host() {
    let Some(probe) = fixture() else { return };
    drop(probe);
    let lifecycle = |fixture: &Fixture, host: &Host, trace: &Path| -> Option<u64> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut client = Client::connect(host);
            let Outcome::Ok(Answer::Created { session, node, .. }) =
                client.request(1, new_session(&fixture.workspace()))
            else {
                panic!("not created");
            };
            let attachment = client.attach(2, &node);
            client.prompt(3, attachment, "delegate this", ANSWER);
            let release = traced(trace);
            assert_eq!(
                client.request(4, Op::ReleaseSession { session }),
                Outcome::Ok(Answer::Released)
            );
            release
        }))
        .ok()
    };
    let baseline = fixture().unwrap();
    baseline.write_script(&spawning_script());
    let trace = baseline.root.join("custody-trace.jsonl");
    let mut host = start_env(&baseline, &[], &[trace_env(&trace)]);
    let release = lifecycle(&baseline, &host, &trace).expect("the baseline released");
    stop(&mut host);
    let commits: Vec<_> = indexed_commits(&trace)
        .into_iter()
        .filter(|(ordinal, ..)| *ordinal > release)
        .collect();
    assert!(!commits.is_empty(), "the release committed nothing");
    for (_, store, store_ordinal) in &commits {
        let cut = format!("cut {store}:{store_ordinal}");
        let fixture = fixture().unwrap();
        fixture.write_script(&spawning_script());
        let cut_trace = fixture.root.join("cut-trace.jsonl");
        if let Some(mut host) =
            try_start(&fixture, &[], &cut_env(&cut_trace, store, *store_ordinal))
        {
            if lifecycle(&fixture, &host, &cut_trace).is_some() {
                stop_or_cut(&mut host);
            } else {
                let status = host.child.take().unwrap().wait().unwrap();
                assert!(!status.success(), "{cut}: the host ended cleanly");
            }
        }
        let host = start(&fixture);
        let host_pid = host.child.as_ref().unwrap().id();
        let mut client = Client::connect(&host);
        let Outcome::Ok(Answer::Created { session, node, .. }) =
            client.request(1, new_session(&fixture.workspace()))
        else {
            panic!("{cut}: the replay made no session");
        };
        assert_eq!(
            client.rows(2)[0].hosted_elsewhere,
            None,
            "{cut}: the dead host reads alive"
        );
        client.attach(3, &node);
        assert_eq!(
            custody_pid(&fixture, &node.tree_id),
            Some(host_pid),
            "{cut}: the next host did not take the tree"
        );
        let (interrupts, decisions) = root_record(&fixture, &session);
        assert!(
            !interrupts.contains(&RootInterruptCause::Handoff)
                && !interrupts.contains(&RootInterruptCause::OwnerLost),
            "{cut}: {interrupts:?}"
        );
        assert_eq!(decisions, 0, "{cut}");
    }
    eprintln!("custody release cuts: {}", commits.len());
}
