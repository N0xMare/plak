use super::super::offline_support::{
    AgentOutcome, AgentProcess, Fixture, WAIT, prompt_frame, quit_frame, reopen_configured,
    reopen_result,
};
use super::{invariants, shapes, summary, trace};
use plakous_offline_crash_harness::commit_trace::{KILL_ENV, TRACE_ENV};
use serde_json::{Value, json};
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::time::Instant;

#[cfg(test)]
#[path = "workflow_fix_tests.rs"]
mod workflow_fix_tests;

#[cfg(test)]
#[path = "gc_export_tests.rs"]
mod gc_export_tests;

#[cfg(test)]
#[path = "view_snapshot.rs"]
mod view_snapshot;

#[cfg(test)]
#[path = "view_idempotence_tests.rs"]
mod view_idempotence_tests;

#[cfg(test)]
#[path = "view_rebuild_laws.rs"]
mod view_rebuild_laws;

#[cfg(test)]
#[path = "composite_laws.rs"]
mod composite_laws;
#[path = "retained_successor_laws.rs"]
mod retained_successor_laws;

fn step_deadline(
    deadline: &mut Instant,
    now: Instant,
    previous: (usize, u64),
    current: (usize, u64),
) -> bool {
    if previous != current {
        *deadline = now + WAIT;
    }
    now >= *deadline
}

#[test]
fn lifecycle_progress_renews_only_the_stalled_step_budget() {
    let start = Instant::now();
    let mut deadline = start + WAIT;
    assert!(!step_deadline(
        &mut deadline,
        start + WAIT / 2,
        (0, 0),
        (1, 0)
    ));
    assert!(!step_deadline(
        &mut deadline,
        start + WAIT,
        (1, 0),
        (1, 100)
    ));
    assert!(!step_deadline(
        &mut deadline,
        start + WAIT * 3 / 2,
        (1, 100),
        (2, 100)
    ));
    assert!(!step_deadline(
        &mut deadline,
        start + WAIT * 2,
        (2, 100),
        (2, 100)
    ));
    assert!(step_deadline(
        &mut deadline,
        start + WAIT * 5 / 2,
        (2, 100),
        (2, 100)
    ));
}

pub(super) fn setup(shape: &str) -> (Fixture, String) {
    let fixture = Fixture::new(&shapes::script(shape));
    (shapes::get(shape).configure)(&fixture);
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("setup hello", |r| r["kind"] == "hello");
    agent.wait_for("setup ready", |r| r["kind"] == "ready");
    (shapes::get(shape).setup)(&mut agent);
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "setup failed: {}", outcome.stderr);
    (fixture, hello["session_id"].as_str().unwrap().to_owned())
}

fn lifecycle(
    fixture: &Fixture,
    session: &str,
    shape: &str,
    cut: Option<&trace::Commit>,
    face_release: Option<&std::path::Path>,
) -> AgentOutcome {
    let _orphans = Orphans(fixture);
    (shapes::get(shape).configure)(fixture);
    let mut command = fixture.command();
    command
        .args(["agent", "--session", session])
        .env(TRACE_ENV, fixture.root.join("commit-trace.jsonl"));
    if shape == "capacity_overflow" {
        command.env(
            "PLAK_TEST_PROVIDER_RELEASE",
            shapes::c1::capacity_release_file(fixture),
        );
    }
    if let Some(release) = face_release {
        command.env("PLAK_TEST_ASSISTANT_DONE_RELEASE", release);
    }
    if let Some(k) = cut {
        command.env(KILL_ENV, format!("{}:{}", k.store, k.store_ordinal));
    }
    let mut agent = AgentProcess::start_command(command, true);
    let mut deadline = Instant::now() + WAIT;
    let mut progress = (0, 0);
    let mut state = shapes::Lifecycle::default();
    loop {
        if agent.child.try_wait().unwrap().is_some() {
            break;
        }
        let lines = agent.snapshot();
        let records = lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        let frame = (shapes::get(shape).lifecycle)(fixture, &records, &mut state);
        let quitting = frame.as_ref().is_some_and(|frame| frame["kind"] == "quit");
        if let Some(frame) = frame {
            let stdin = agent.stdin.as_mut().unwrap();
            // SIGKILL can race a frame write after a durable startup commit.
            if writeln!(stdin, "{frame}")
                .and_then(|()| stdin.flush())
                .is_err()
            {
                break;
            }
        }
        if let Some(release) = face_release
            && !release.exists()
            && records.iter().any(|row| row["kind"] == "assistant_done")
        {
            let pending = release.with_extension("pending");
            std::fs::write(
                &pending,
                if quitting {
                    b"quit".as_slice()
                } else {
                    b"release".as_slice()
                },
            )
            .unwrap();
            std::fs::rename(pending, release).unwrap();
        }
        let observed = (
            lines.len(),
            std::fs::metadata(fixture.root.join("commit-trace.jsonl"))
                .map_or(0, |metadata| metadata.len()),
        );
        if step_deadline(&mut deadline, Instant::now(), progress, observed) {
            fixture.preserve();
            panic!(
                "traced lifecycle made no progress within step deadline; fixture={}; records={records:?}",
                fixture.root.display()
            );
        }
        progress = observed;
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    agent.finish()
}

pub(super) fn pilot(seed: &Fixture, session: &str, shape: &str) -> Vec<trace::Commit> {
    let mut pilots = Vec::new();
    let mut fixtures = Vec::new();
    for iteration in 0..2 {
        let fixture = seed.fork(&shapes::script(shape));
        // Expose the answer marker before face idle in the first twin; the
        // second drains busy=false before inspecting any subsequent input.
        let face_release = fixture.root.join("face-release");
        if iteration == 1 {
            std::fs::write(&face_release, b"idle").unwrap();
        }
        let outcome = lifecycle(&fixture, session, shape, None, Some(&face_release));
        assert!(outcome.status.success(), "pilot failed: {}", outcome.stderr);
        let state = summary::read_converged(&fixture);
        (shapes::get(shape).assert_live)(&fixture, &outcome, &state);
        pilots.push(trace::read(&fixture.root.join("commit-trace.jsonl")));
        fixtures.push((fixture, outcome));
    }
    if trace::sequences(&pilots[0]) != trace::sequences(&pilots[1]) {
        for (fixture, outcome) in &fixtures {
            fixture.preserve();
            std::fs::write(fixture.root.join("pilot.jsonl"), outcome.lines.join("\n")).unwrap();
            std::fs::write(fixture.root.join("pilot.stderr"), &outcome.stderr).unwrap();
            eprintln!("divergent pilot fixture: {}", fixture.root.display());
        }
    }
    assert_eq!(
        trace::sequences(&pilots[0]),
        trace::sequences(&pilots[1]),
        "DIVERGENT per-store twin pilots; shape refused"
    );
    let mut pilot = pilots.remove(0);
    pilot.sort_by_key(|commit| (commit.store.clone(), commit.store_ordinal));
    pilot
}

pub(super) fn cut(
    seed: &Fixture,
    session: &str,
    shape: &str,
    baseline: &summary::Summary,
    pilot: &[trace::Commit],
    target: &trace::Commit,
) -> Value {
    let (fixture, commits) = kill_fixture(seed, session, shape, pilot, target);
    let _orphans = Orphans(&fixture);
    let before = summary::read(&fixture);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cut_after_kill(&fixture, session, shape, &commits, &before, baseline)
    }));
    match result {
        Ok(row) => row,
        Err(error) => {
            fixture.preserve();
            json!({"outcome": "UNCLASSIFIED", "invariants": false,
                "finding": super::panic_text(&error), "before": before,
                "fixture_directory": fixture.root, "global_order": commits})
        }
    }
}

#[test]
fn capacity_denial_and_unsent_fold_cuts_share_one_pilot() {
    let shape = "capacity_overflow";
    let (seed, session) = setup(shape);
    let baseline = summary::read_converged(&seed);
    let pilot = pilot(&seed, &session, shape);
    for store in ["product", "runtime"] {
        let target = find_capacity_denial_cut(&pilot, store, &baseline, |commit| {
            let (fixture, _) = kill_fixture(&seed, &session, shape, &pilot, commit);
            let _orphans = Orphans(&fixture);
            summary::read(&fixture)
        });
        let row = cut(&seed, &session, shape, &baseline, &pilot, target);
        assert_eq!(row["invariants"], true, "{row}");
        assert_eq!(row["outcome"], "COMPLETED_ONCE", "{row}");
    }
    // The overflow-fold decision is durable, but its summary is still unsent.
    let target = pilot
        .iter()
        .find(|commit| {
            if commit.store != "runtime" || commit.label != "commit" {
                return false;
            }
            let (fixture, _) = kill_fixture(&seed, &session, shape, &pilot, commit);
            let _orphans = Orphans(&fixture);
            let state = summary::read(&fixture);
            state.runtime_verdict == "RecoversAbandonedTurn"
                && state
                    .ledger
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["kind"] == "provider" && row["state"] == "completed")
        })
        .expect("recorded overflow-fold decision signature");
    let row = cut(&seed, &session, shape, &baseline, &pilot, target);
    assert_eq!(row["invariants"], true, "{row}");
    assert_eq!(row["outcome"], "ABANDONED{nothing_started}", "{row}");
}

fn find_capacity_denial_cut<'a>(
    pilot: &'a [trace::Commit],
    store: &str,
    baseline: &summary::Summary,
    mut snapshot: impl FnMut(&trace::Commit) -> summary::Summary,
) -> &'a trace::Commit {
    // Several commits have the same SQL signature. Decode their exact heads
    // instead of giving a transaction count semantic meaning.
    pilot
        .iter()
        .find(|commit| {
            let signature = commit.store == store
                && match store {
                    "product" => commit.label == "unlabelled" && commit.tables == ["sessions"],
                    "runtime" => {
                        commit.label == "commit"
                            && commit.tables.iter().any(|table| table == "commit_receipts")
                    }
                    _ => false,
                };
            if !signature {
                return false;
            }
            let state = snapshot(commit);
            state.runtime_verdict == "OpensQuiescent"
                && state.capacity_terminals == usize::from(store == "product")
                && state.ledger.as_array().unwrap().iter().any(|row| {
                    row["kind"] == "command"
                        && row["state"] == "finalized"
                        && !baseline
                            .ledger
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|old| old["operation"] == row["operation"])
                })
        })
        .expect("capacity terminal commit signature and facts")
}

#[test]
fn capacity_cut_selection_uses_facts_not_transaction_ordinals() {
    for store in ["product", "runtime"] {
        let commits = [2, 120].map(|ordinal| trace::Commit {
            ordinal,
            store: store.to_owned(),
            store_index: ordinal,
            store_ordinal: ordinal,
            label: if store == "product" {
                "unlabelled"
            } else {
                "commit"
            }
            .to_owned(),
            tables: if store == "product" {
                vec!["sessions".to_owned()]
            } else {
                vec!["commit_receipts".to_owned()]
            },
            indexed: true,
        });
        let baseline = summary::Summary {
            ledger: json!([]),
            ..summary::Summary::default()
        };
        let selected = find_capacity_denial_cut(&commits, store, &baseline, |commit| {
            summary::Summary {
                runtime_verdict: "OpensQuiescent".to_owned(),
                capacity_terminals: usize::from(store == "product"),
                ledger: json!([{"kind":"command", "operation":1, "state":if commit.ordinal == 120 { "finalized" } else { "prepared" }}]),
                ..summary::Summary::default()
            }
        });
        assert_eq!(selected.ordinal, 120);
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one cut's relaunch sequence reads best as a single ordered procedure"
)]
fn cut_after_kill(
    fixture: &Fixture,
    session: &str,
    shape: &str,
    commits: &[trace::Commit],
    before: &summary::Summary,
    baseline: &summary::Summary,
) -> Value {
    let rehearsal = summary::rehearse(fixture, session);
    let dispatch_before = invariants::dispatches(fixture);
    let summaries_before = invariants::summary_dispatches(fixture);
    let original = original_facts(before, baseline);
    let expected = shapes::get(shape);
    let capacity = expected.terminal == shapes::ExpectedTerminal::CapacityDenied;
    let requeued = !original.command && original.queued && (!capacity || before.original_queued);
    if capacity {
        invariants::capacity_publication(before, dispatch_before.len());
    }
    let mut followup = if requeued {
        requeued_script(fixture, shape)
    } else {
        shapes::get(shape).followup()
    };
    let queued_admitted = capacity
        && before
            .ledger
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "command")
            .count()
            > 1;
    if capacity && !requeued && before.queued_prompt_pending && !queued_admitted {
        followup["steps"] = json!([
            {"output": {"text": shapes::c1::QUEUED}},
            {"output": {"text": shapes::FOLLOWUP}}
        ]);
    }
    followup["repeat_last_step"] = json!(true);
    fixture.write_script(&followup);
    (shapes::get(shape).configure)(fixture);
    let provider_release =
        (capacity && requeued).then(|| shapes::c1::capacity_release_file(fixture));
    let mut reopened =
        match traced_reopen(fixture, session, "relaunch-1", provider_release.as_deref()) {
            Ok(agent) => agent,
            Err(outcome) => {
                let mut result = refused(
                    fixture,
                    session,
                    shape,
                    before,
                    &rehearsal,
                    &outcome,
                    &dispatch_before,
                );
                result["global_order"] = json!(commits);
                result["global_ordinal"] = json!(commits.last().unwrap().ordinal);
                return result;
            }
        };
    let recovered = summary::read_reopened(fixture);
    if shape == "scheduled_compaction_workflow" {
        super::workflow_recovery::assert_admitted_abandonment(fixture, before, &recovered);
    }
    assert_recovered_artifact(shape, before, &recovered);
    let first_summary_dispatch = invariants::recovery_dispatches(
        shape,
        before,
        &recovered,
        &dispatch_before,
        &invariants::dispatches(fixture),
    );
    let summaries_recovered = invariants::summary_dispatches(fixture);
    if shape == "scheduled_compaction_workflow" {
        invariants::summary_once(
            &summaries_before,
            &summaries_recovered,
            first_summary_dispatch,
        );
    } else {
        assert_eq!(
            summaries_before, summaries_recovered,
            "runtime recovery did inference"
        );
    }
    let outcome = classify_expected(before, &recovered, baseline, &rehearsal, expected);
    if capacity
        && recovered.capacity_terminals > before.capacity_terminals
        && recovered.queued_prompt_pending
    {
        assert_eq!(
            recovered.drain, "restored_paused",
            "capacity pause differs from pilot"
        );
    }
    if shape == "steer_accepted_mid_turn" {
        let pending_states = before
            .ledger
            .as_array()
            .unwrap()
            .iter()
            .filter(|record| record["kind"] == "command" && record["operation"] != 1)
            .map(|record| record["state"].as_str().unwrap())
            .collect::<Vec<_>>();
        if pending_states.contains(&"completed") {
            assert_eq!(
                recovered.unapplied_steering_notices, 1,
                "an unfinalized Completed steer requires exactly one truthful unapplied notice"
            );
        }
        if (outcome == "COMPLETED_ONCE" || outcome.starts_with("ABANDONED"))
            && pending_states
                .iter()
                .any(|state| matches!(*state, "prepared" | "dispatched" | "completed"))
        {
            assert_eq!(
                recovered.unapplied_steering_notices, 1,
                "unapplied steering needs exactly one notice"
            );
        }
    }
    let mut requeued_lifecycle = Value::Null;
    if outcome == "REQUEUED" {
        assert!(requeued, "unexpected restored prompt behind admitted root");
        reopened.send(&json!({"v": 1, "kind": "resume_queue"}));
        requeued_lifecycle = resume_original(&mut reopened, fixture, shape);
        assert_eq!(
            expected.published(&summary::read_converged(fixture)),
            1,
            "requeued original publication"
        );
    }
    let queued_pending = capacity && outcome != "REQUEUED" && recovered.queued_prompt_pending;
    if queued_pending {
        assert!(
            recovered.capacity_terminals == 1 || outcome.starts_with("ABANDONED"),
            "queued prompt passed unpublished terminal"
        );
        reopened.send(&json!({"v": 1, "kind": "resume_queue"}));
        reopened.wait_for_completed_turns(1);
        assert_eq!(summary::read_converged(fixture).queued_answers, 1);
    }
    reopened.send(&prompt_frame("crash-sweep-followup-prompt"));
    if recovered.drain == "restored_paused" {
        reopened.send(&json!({"v": 1, "kind": "resume_queue"}));
    }
    reopened.wait_for_completed_turns(if outcome == "REQUEUED" || queued_pending {
        2
    } else {
        1
    });
    reopened.send(&quit_frame());
    let first = reopened.finish();
    assert!(first.status.success(), "follow-up failed: {}", first.stderr);
    let stable = summary::read_converged(fixture);
    invariants::quiescent(&stable);
    assert_eq!(
        expected.published(&stable),
        expected.published(&recovered) + usize::from(outcome == "REQUEUED"),
        "original answer disappeared or replayed during follow-up"
    );
    if capacity {
        assert!(stable.capacity_terminals <= 1 && stable.queued_answers <= 1);
        assert_eq!(
            stable.drain, "live_auto_drain",
            "the follow-up acceptance must clear the joined pause cause"
        );
    }
    invariants::no_duplicate_dispatch(fixture);
    invariants::notices_once(fixture);
    let dispatched = invariants::dispatches(fixture);
    let summary_dispatches = invariants::summary_dispatches(fixture);
    if shape == "scheduled_compaction_workflow" {
        assert!(
            summary_dispatches.len() <= 1,
            "duplicate physical summary dispatch across row"
        );
        assert!(
            summary_dispatches
                .iter()
                .all(|id| dispatched.contains(&("ProviderDispatched".to_owned(), id.clone())))
        );
    }
    if outcome == "NOT_ADMITTED" {
        assert_eq!(
            dispatched.len(),
            dispatch_before.len() + 1,
            "NOT_ADMITTED follow-up dispatched other work"
        );
    }
    let mut second = traced_reopen(fixture, session, "relaunch-2", None)
        .unwrap_or_else(|outcome| panic!("second reopen refused: {:?}", outcome.lines));
    summary::read_reopened(fixture);
    second.send(&quit_frame());
    let second_outcome = second.finish();
    assert!(second_outcome.status.success());
    invariants::job_notices_once(before, &first, &second_outcome);
    let mut converged = summary::read_converged(fixture);
    let second_revision = converged.revision;
    // Idle attach/quit advances the CAS revision without changing turn authority.
    converged.revision = stable.revision;
    assert_eq!(
        stable, converged,
        "second relaunch did not semantically converge"
    );
    assert_eq!(
        dispatched,
        invariants::dispatches(fixture),
        "second relaunch dispatched"
    );
    assert_eq!(
        summary_dispatches,
        invariants::summary_dispatches(fixture),
        "second relaunch did inference"
    );
    invariants::notices_once(fixture);
    json!({"outcome": outcome, "invariants": true, "before": before, "after_recovery": recovered, "after_followup": stable, "rehearsal": rehearsal, "dispatches": dispatched, "summary_dispatches": summary_dispatches, "first_summary_dispatch": first_summary_dispatch, "second_relaunch_converged": true, "second_revision": second_revision, "global_order": commits, "global_ordinal": commits.last().unwrap().ordinal, "requeued_lifecycle": requeued_lifecycle})
}

fn requeued_script(fixture: &Fixture, shape: &str) -> Value {
    let mut script: Value = serde_json::from_slice(&std::fs::read(fixture.script()).unwrap())
        .expect("post-setup configured original script");
    if shape == "busy_quit_escalates" {
        // Only the cut lifecycle is cancellation-deaf. A never-admitted original
        // resumes normally so its queued recovery can finish before the follow-up.
        script["steps"] = json!([{"output": {"text": shapes::ANSWER}}]);
    }
    script["steps"]
        .as_array_mut()
        .unwrap()
        .push(shapes::get(shape).followup()["steps"][0].clone());
    script
}

#[test]
fn requeued_seeded_shape_uses_configured_post_setup_steps() {
    let shape = "scheduled_compaction_workflow";
    let fixture = Fixture::new(&shapes::script(shape));
    let mut configured = shapes::script(shape);
    configured["steps"] = json!([{"output": {"text": shapes::ANSWER}}]);
    fixture.write_script(&configured);
    let requeued = requeued_script(&fixture, shape);
    assert_eq!(requeued["steps"].as_array().unwrap().len(), 2);
    assert_eq!(requeued["steps"][0]["output"]["text"], shapes::ANSWER);
    assert_eq!(requeued["steps"][1]["output"]["text"], shapes::FOLLOWUP);
    assert_eq!(requeued["scheduled_compaction"], true);
    assert_eq!(
        requeued["maximum_projected_input"],
        configured["maximum_projected_input"]
    );
}

fn resume_original(agent: &mut AgentProcess, fixture: &Fixture, shape: &str) -> Value {
    let mut state = shapes::Lifecycle {
        sent: true,
        ..shapes::Lifecycle::default()
    };
    let deadline = Instant::now() + WAIT;
    let mut commands = Vec::new();
    loop {
        let records = agent
            .snapshot()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        if let Some(frame) = (shapes::get(shape).lifecycle)(fixture, &records, &mut state) {
            if frame["kind"] == "quit" {
                let acknowledgements = records
                    .iter()
                    .filter(|row| row["kind"] == "steer_ack")
                    .map(|row| row["state"].clone())
                    .collect::<Vec<_>>();
                return json!({"commands": commands, "steer_ack": acknowledgements});
            }
            commands.push(frame["kind"].clone());
            agent.send(&frame);
        }
        assert!(
            Instant::now() < deadline,
            "requeued shape lifecycle exceeded step deadline"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn assert_recovered_artifact(shape: &str, before: &summary::Summary, recovered: &summary::Summary) {
    if shape != "sealed_family_artifacts" || recovered.original_answers != 1 {
        return;
    }
    let artifact = recovered
        .family_artifact
        .as_ref()
        .expect("recovered sealed artifact");
    assert_eq!(artifact["family"], "responses");
    assert_eq!(artifact["fixture_native"], true);
    if before.original_answers == 1 && before.family_artifact.is_some() {
        assert_eq!(
            before.family_artifact, recovered.family_artifact,
            "native artifact changed during recovery"
        );
    }
}

#[test]
fn converged_summary_projects_debt_without_changing_cut_trace() {
    let shape = "final_only";
    let (seed, session) = setup(shape);
    let fixture = seed.fork(&shapes::script(shape));
    let outcome = lifecycle(&fixture, &session, shape, None, None);
    assert!(outcome.status.success(), "{}", outcome.stderr);
    let trace_path = fixture.root.join("commit-trace.jsonl");
    let traced = std::fs::read(&trace_path).unwrap();
    let mut product = plak::store::ProductStore::open(&summary::paths(&fixture)).unwrap();
    product.delete_views(&session).unwrap();
    drop(product);

    let cut = summary::read(&fixture);
    assert_eq!(cut.original_answers, 0);
    assert_ne!(cut.locator.as_ref(), Some(&cut.head));
    assert_eq!(
        summary::read(&fixture),
        cut,
        "cut reads must leave debt intact"
    );
    let converged = summary::read_converged(&fixture);
    assert_eq!(converged.locator.as_ref(), Some(&converged.head));
    assert!(converged.phase.is_none());
    assert!(converged.obligations.is_empty());
    assert_eq!(converged.original_answers, 1);
    assert_eq!(
        converged.head, cut.head,
        "projection changed runtime authority"
    );
    assert_eq!(summary::read_converged(&fixture), converged);
    assert_eq!(std::fs::read(trace_path).unwrap(), traced);
}

#[test]
fn reopened_summary_refuses_to_mask_unpaid_projection_debt() {
    let (seed, session) = setup("final_only");
    let fixture = seed.fork(&shapes::script("final_only"));
    let outcome = lifecycle(&fixture, &session, "final_only", None, None);
    assert!(outcome.status.success(), "{}", outcome.stderr);
    let mut product = plak::store::ProductStore::open(&summary::paths(&fixture)).unwrap();
    product.delete_views(&session).unwrap();
    drop(product);
    let before = summary::read(&fixture);
    let refusal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        summary::read_reopened(&fixture)
    }));
    assert!(refusal.is_err(), "the harness must not repair reopen debt");
    let store = plak::store::session_records::SqliteSessionStore::new(summary::paths(&fixture));
    assert!(
        plak::store::session_records::SessionRecordStore::load_views(&store, &session)
            .unwrap()
            .is_none()
    );
    assert_eq!(summary::read(&fixture), before);
}

#[test]
fn reopened_summary_rejects_a_cursor_behind_the_runtime_tail() {
    use plakous_hosted::sessions::projector::{SessionViews, ViewWrites};

    let (seed, session) = setup("final_only");
    let paths = summary::paths(&seed);
    let mut product = plak::store::ProductStore::open(&paths).unwrap();
    let earlier = product.load_views(&session).unwrap().unwrap();
    drop(product);
    let fixture = seed.fork(&shapes::script("final_only"));
    let outcome = lifecycle(&fixture, &session, "final_only", None, None);
    assert!(outcome.status.success(), "{}", outcome.stderr);
    let converged = summary::read_converged(&fixture);
    let mut product = plak::store::ProductStore::open(&summary::paths(&fixture)).unwrap();
    assert!(product.load_views(&session).unwrap().unwrap().cursor > earlier.cursor);
    product.delete_views(&session).unwrap();
    product
        .commit_projection_views(
            &session,
            &ViewWrites::between(&SessionViews::empty(), &earlier),
            &earlier,
        )
        .unwrap();
    drop(product);
    let before = summary::read(&fixture);
    let refusal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        summary::read_reopened(&fixture)
    }));
    assert!(
        refusal.is_err(),
        "an existing view must also reach the tail"
    );
    let store = plak::store::session_records::SqliteSessionStore::new(summary::paths(&fixture));
    assert_eq!(
        plak::store::session_records::SessionRecordStore::load_views(&store, &session).unwrap(),
        Some(earlier)
    );
    assert_eq!(summary::read(&fixture), before);
    assert_eq!(summary::read_converged(&fixture), converged);
}

#[test]
fn summary_queue_joins_consumed_submissions_without_changing_the_cut_trace() {
    let shape = "final_only";
    let (seed, session) = setup(shape);
    let pilot = pilot(&seed, &session, shape);
    let accepted = pilot
        .iter()
        .find(|commit| commit.store == "runtime" && commit.label == "commit")
        .expect("root admission commit");
    let (fixture, _) = kill_fixture(&seed, &session, shape, &pilot, accepted);
    let product = rusqlite::Connection::open_with_flags(
        summary::paths(&fixture).product_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert!(
        product
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM queue_items WHERE text=?1)",
                ["crash-sweep-original-prompt"],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    );
    drop(product);
    let trace_path = fixture.root.join("commit-trace.jsonl");
    let traced = std::fs::read(&trace_path).unwrap();
    let state = summary::read_converged(&fixture);
    assert!(state.queue.is_empty());
    assert!(!state.original_queued);
    assert_eq!(std::fs::read(&trace_path).unwrap(), traced);
}

#[test]
fn converged_summary_projects_beside_live_owner() {
    let (fixture, session) = setup("final_only");
    let mut opened = traced_reopen(&fixture, &session, "live-summary", None)
        .unwrap_or_else(|outcome| panic!("{}", outcome.stderr));
    opened.send(&prompt_frame("crash-sweep-original-prompt"));
    opened.wait_for_completed_turns(1);
    let converged = summary::read_converged(&fixture);
    assert_eq!(converged.original_answers, 1);
    assert_eq!(converged.locator.as_ref(), Some(&converged.head));
    opened.send(&quit_frame());
    assert!(opened.finish().status.success());
}

struct Orphans<'a>(&'a Fixture);
impl Drop for Orphans<'_> {
    fn drop(&mut self) {
        for entry in std::fs::read_dir(&self.0.root).unwrap().flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "pid")
                && let Ok(pid) = std::fs::read_to_string(entry.path())
                    .unwrap_or_default()
                    .trim()
                    .parse::<i32>()
                && pid > 1
            {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }
}

#[test]
fn refused_stale_head_jsonl_preserves_recovery_required_exit() {
    let (seed, session) = setup("final_only");
    let fixture = seed.fork(&shapes::script("final_only"));
    let completed = lifecycle(&fixture, &session, "final_only", None, None);
    assert!(completed.status.success());
    let before = summary::read(&fixture);
    // Accepted command bytes and operation request bytes must never cross.
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(plakous_offline_crash_harness::seed_crossed_root(
            &summary::paths(&fixture).runtime_database(),
            before.head.parse().unwrap(),
        ))
        .unwrap();
    // Project this artificial seed: spec 02 section 4.2 step 0 precedes the exact open.
    let crossed = summary::read_converged(&fixture);
    let refused = reopen_result(&fixture, &session)
        .err()
        .expect("unfinished stale head unexpectedly opened");
    assert_eq!(
        refused.status.code(),
        Some(5),
        "stdout={:?}; stderr={}",
        refused.lines,
        refused.stderr
    );
    assert!(
        refused
            .records()
            .iter()
            .any(|record| record["kind"] == "error")
    );
    refused.assert_exactly_one_terminal_record();
    let after = summary::read_converged(&fixture);
    if after != crossed {
        fixture.preserve();
        eprintln!(
            "refusal changed durable authority; fixture={}",
            fixture.root.display()
        );
    }
    assert_eq!(after, crossed, "refusal changed durable authority");
}

fn traced_reopen(
    fixture: &Fixture,
    session: &str,
    label: &str,
    provider_release: Option<&std::path::Path>,
) -> Result<AgentProcess, AgentOutcome> {
    reopen_configured(fixture, session, |command, attempt| {
        command.env(
            TRACE_ENV,
            fixture.root.join(format!("{label}-{attempt}.jsonl")),
        );
        if let Some(release) = provider_release {
            command.env("PLAK_TEST_PROVIDER_RELEASE", release);
        }
    })
}

fn kill_fixture(
    seed: &Fixture,
    session: &str,
    shape: &str,
    pilot: &[trace::Commit],
    target: &trace::Commit,
) -> (Fixture, Vec<trace::Commit>) {
    let mut fixture = None;
    for attempt in 0..3 {
        let fresh = seed.fork(&shapes::script(shape));
        let outcome = lifecycle(&fresh, session, shape, Some(target), None);
        let commits = trace::read(&fresh.root.join("commit-trace.jsonl"));
        if outcome.status.signal() == Some(9) && trace::valid_cut(pilot, &commits, target) {
            fixture = Some((fresh, commits));
            break;
        }
        assert!(
            attempt < 2,
            "DIVERGENT cut {}:{}: signal={:?}, code={:?}, actual={:?}, expected={:?}, stderr={}",
            target.store,
            target.store_ordinal,
            outcome.status.signal(),
            outcome.status.code(),
            trace::sequences(&commits),
            trace::sequences(pilot),
            outcome.stderr
        );
    }
    fixture.unwrap()
}

fn refused(
    fixture: &Fixture,
    session: &str,
    shape: &str,
    before: &summary::Summary,
    rehearsal: &Value,
    outcome: &AgentOutcome,
    dispatch_before: &[(String, String)],
) -> Value {
    assert_eq!(
        outcome.status.code(),
        Some(5),
        "refusal was not typed exit 5: stdout={:?}, stderr={}",
        outcome.lines,
        outcome.stderr
    );
    let after_refusal = summary::read(fixture);
    assert_eq!(
        before, &after_refusal,
        "refused startup changed decoded durable authority"
    );
    assert_eq!(
        dispatch_before,
        invariants::dispatches(fixture),
        "refusal dispatched"
    );
    assert!(
        !rehearsal["success"].as_bool().unwrap(),
        "rehearsal accepted actual refusal"
    );
    let signature = summary::refusal_signature(shape, before);
    let allowlist: Value = serde_json::from_str(include_str!("refusal_allowlist.json")).unwrap();
    let name = allowlist[&signature]["name"]
        .as_str()
        .unwrap_or("not_allowlisted");
    assert!(
        summary::outcome_allows_verdict("REFUSED", rehearsal["verdict"].as_str().unwrap()),
        "refusal outcome/verdict mismatch"
    );
    let second = traced_reopen(fixture, session, "relaunch-2", None)
        .err()
        .expect("refusal did not converge");
    assert_eq!(second.status.code(), Some(5));
    assert_eq!(
        before,
        &summary::read(fixture),
        "second refusal changed decoded authority"
    );
    json!({"outcome": format!("REFUSED{{{name}}}"), "invariants": name != "not_allowlisted", "before": before, "rehearsal": rehearsal, "refusal_signature": signature, "after_refusal": after_refusal, "refusal_exit": 5, "refusal_stderr": outcome.stderr, "second_relaunch_converged": true})
}

fn classify(
    before: &summary::Summary,
    recovered: &summary::Summary,
    baseline: &summary::Summary,
    rehearsal: &Value,
) -> String {
    classify_expected(
        before,
        recovered,
        baseline,
        rehearsal,
        shapes::get("final_only"),
    )
}

fn classify_expected(
    before: &summary::Summary,
    recovered: &summary::Summary,
    baseline: &summary::Summary,
    rehearsal: &Value,
    expected: &shapes::Shape,
) -> String {
    if expected.terminal == shapes::ExpectedTerminal::CapacityDenied
        && recovered.capacity_terminals == 0
        && recovered.abandonment_claims.is_empty()
        && [before, recovered].iter().any(|state| {
            state
                .ledger
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["kind"] == "command")
                .is_some_and(|row| row["state"] == "finalized")
        })
    {
        panic!(
            "UNCLASSIFIED finalized capacity terminal was not published: {before:?} -> {recovered:?}"
        );
    }
    let published = match expected.terminal {
        shapes::ExpectedTerminal::Answer => completed_original_publications(recovered, baseline),
        shapes::ExpectedTerminal::CapacityDenied => expected.published(recovered),
    };
    let outcome = if published == 1 {
        "COMPLETED_ONCE".to_owned()
    } else if let Some(claim) = recovered.abandonment_claims.first() {
        assert!(
            !before.durable_answer,
            "lost durable answer via abandonment"
        );
        format!("ABANDONED{{{claim}}}")
    } else if original_facts(recovered, baseline).queued
        && (expected.terminal == shapes::ExpectedTerminal::Answer || recovered.original_queued)
    {
        assert_eq!(recovered.drain, "restored_paused");
        "REQUEUED".to_owned()
    } else if !original_facts(before, baseline).command
        && !original_facts(before, baseline).submission
        && !before.durable_answer
    {
        "NOT_ADMITTED".to_owned()
    } else {
        panic!("UNCLASSIFIED cut: {before:?} -> {recovered:?}")
    };
    assert_eq!(
        rehearsal["verdict"], before.runtime_verdict,
        "rehearsal outcome parity"
    );
    summary::assert_outcome_verdict(&outcome, before, baseline);
    outcome
}

#[test]
fn capacity_denial_rejects_a_double_terminal_disguised_as_queued_recovery() {
    let baseline = summary::Summary {
        ledger: json!([]),
        ..summary::Summary::default()
    };
    let before = summary::Summary {
        ledger: json!([{ "kind": "command", "operation": 1, "state": "completed" }]),
        phase: Some("Finalization".to_owned()),
        ..summary::Summary::default()
    };
    let recovered = summary::Summary {
        capacity_terminals: 1,
        abandonment_claims: vec!["model_request".to_owned()],
        ..before.clone()
    };
    assert!(
        std::panic::catch_unwind(|| classify_expected(
            &before,
            &recovered,
            &baseline,
            &json!({"verdict": "RecoversAbandonedTurn"}),
            shapes::get("capacity_overflow")
        ))
        .is_err(),
        "the original cannot both publish capacity and be abandoned"
    );
}

#[test]
fn capacity_denial_missing_publication_after_runtime_finalization_is_a_finding() {
    let baseline = summary::Summary {
        ledger: json!([]),
        ..summary::Summary::default()
    };
    let before = summary::Summary {
        ledger: json!([{ "kind": "command", "operation": 1, "state": "completed" }]),
        phase: Some("Finalization".to_owned()),
        ..summary::Summary::default()
    };
    let recovered = summary::Summary {
        ledger: json!([{ "kind": "command", "operation": 1, "state": "finalized" }]),
        ..summary::Summary::default()
    };
    let error = std::panic::catch_unwind(|| {
        classify_expected(
            &before,
            &recovered,
            &baseline,
            &json!({"verdict": "RecoversSavedTerminal"}),
            shapes::get("capacity_overflow"),
        )
    })
    .expect_err("missing capacity publication must fail");
    assert!(super::panic_text(&error).contains("finalized capacity terminal was not published"));
}

#[test]
fn capacity_denial_abandoned_command_is_not_a_missing_capacity_publication() {
    let baseline = summary::Summary {
        ledger: json!([]),
        ..summary::Summary::default()
    };
    let before = summary::Summary {
        runtime_verdict: "RecoversAbandonedTurn".to_owned(),
        ledger: json!([{ "kind": "command", "operation": 1, "state": "prepared" }]),
        phase: Some("DispatchEvidence".to_owned()),
        ..summary::Summary::default()
    };
    let recovered = summary::Summary {
        ledger: json!([{ "kind": "command", "operation": 1, "state": "finalized" }]),
        abandonment_claims: vec!["nothing_started".to_owned()],
        ..summary::Summary::default()
    };
    assert_eq!(
        classify_expected(
            &before,
            &recovered,
            &baseline,
            &json!({"verdict": "RecoversAbandonedTurn"}),
            shapes::get("capacity_overflow"),
        ),
        "ABANDONED{nothing_started}"
    );
}

#[test]
fn capacity_denial_classifies_only_the_expected_terminal() {
    let baseline = summary::Summary {
        ledger: json!([]),
        ..summary::Summary::default()
    };
    let before = summary::Summary {
        runtime_verdict: "RecoversSavedTerminal".to_owned(),
        ledger: json!([{ "kind": "command", "operation": 1, "state": "completed" }]),
        phase: Some("Finalization".to_owned()),
        ..summary::Summary::default()
    };
    let recovered = summary::Summary {
        ledger: before.ledger.clone(),
        capacity_terminals: 1,
        drain: "restored_paused".to_owned(),
        ..summary::Summary::default()
    };
    assert_eq!(
        classify_expected(
            &before,
            &recovered,
            &baseline,
            &json!({"verdict": "RecoversSavedTerminal"}),
            shapes::get("capacity_overflow")
        ),
        "COMPLETED_ONCE"
    );
    assert!(
        std::panic::catch_unwind(|| classify_expected(
            &before,
            &recovered,
            &baseline,
            &json!({"verdict": "RecoversSavedTerminal"}),
            shapes::get("final_only")
        ))
        .is_err()
    );
}

#[test]
fn capacity_publication_does_not_requeue_a_later_prompt_or_own_its_recovery() {
    let baseline = summary::Summary {
        ledger: json!([]),
        ..summary::Summary::default()
    };
    let before = summary::Summary {
        ledger: json!([{ "kind": "command", "operation": 1, "state": "finalized" }]),
        queue: vec!["second-prompt".to_owned()],
        drain: "restored_paused".to_owned(),
        ..summary::Summary::default()
    };
    assert!(
        std::panic::catch_unwind(|| classify_expected(
            &before,
            &before,
            &baseline,
            &json!({"verdict": "OpensQuiescent"}),
            shapes::get("capacity_overflow")
        ))
        .is_err()
    );
    let mut recovered = before.clone();
    recovered.capacity_terminals = 1;
    let mut before = recovered.clone();
    before.runtime_verdict = "RecoversAbandonedTurn".to_owned();
    before.head_turn = Some("later-turn".into());
    before.submissions = vec![
        json!({"command": "original", "kind": "root_start", "state": "accepted"}),
        json!({"command": "later", "kind": "root_start", "state": "pending"}),
    ];
    before.root_lifecycles = vec![
        json!({"command": "original", "turn": "original-turn", "sequence": 1, "state": "finalized"}),
        json!({"command": "later", "turn": "later-turn", "sequence": 2, "state": "running"}),
    ];
    recovered
        .abandonment_claims
        .push("model_request".to_owned());
    assert_eq!(
        classify_expected(
            &before,
            &recovered,
            &baseline,
            &json!({"verdict": "RecoversAbandonedTurn"}),
            shapes::get("capacity_overflow")
        ),
        "COMPLETED_ONCE"
    );
}

fn completed_original_publications(
    recovered: &summary::Summary,
    baseline: &summary::Summary,
) -> usize {
    recovered
        .published_completed_commands
        .iter()
        .filter(|command| !baseline.published_completed_commands.contains(command))
        .count()
}

struct OriginalFacts {
    command: bool,
    submission: bool,
    queued: bool,
}

fn original_facts(state: &summary::Summary, baseline: &summary::Summary) -> OriginalFacts {
    OriginalFacts {
        command: state.ledger.as_array().unwrap().iter().any(|row| {
            row["kind"] == "command"
                && !baseline
                    .ledger
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|old| old["operation"] == row["operation"])
        }),
        submission: state.submissions.iter().any(|row| {
            !baseline
                .submissions
                .iter()
                .any(|old| old["command"] == row["command"])
        }),
        queued: state.queue.iter().any(|id| !baseline.queue.contains(id)),
    }
}

#[test]
fn seeded_roots_do_not_admit_the_crash_original() {
    let baseline = summary::Summary {
        runtime_verdict: "OpensQuiescent".to_owned(),
        ledger: json!([
        {"kind": "command", "operation": 1, "state": "finalized"},
        {"kind": "provider", "operation": 2, "state": "finalized"}]),
        submissions: vec![json!({"command": "seed-submission", "state": "accepted"})],
        queue: vec!["seed-queue".to_owned()],
        ..summary::Summary::default()
    };
    let rehearsal = json!({"verdict": "OpensQuiescent"});
    assert_eq!(
        classify(&baseline, &baseline, &baseline, &rehearsal),
        "NOT_ADMITTED"
    );
    let mut queued = baseline.clone();
    queued.queue.push("original-queue".to_owned());
    queued.drain = "restored_paused".to_owned();
    assert_eq!(
        classify(&queued, &queued, &baseline, &rehearsal),
        "REQUEUED"
    );
    queued
        .ledger
        .as_array_mut()
        .unwrap()
        .push(json!({"kind": "command", "operation": 3, "state": "finalized"}));
    assert!(
        original_facts(&queued, &baseline).command,
        "a finalized original remains distinct from a finalized seed"
    );
}
