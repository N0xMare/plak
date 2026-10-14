//! Deterministic indexed `SQLite` commit cuts of the real offline product.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private real-process test harness"
)]
mod coverage;
mod driver;
mod invariants;
mod publication;
mod shapes;
mod summary;
mod table;
mod trace;
mod workflow_recovery;

use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn durability_subset() {
    let jobs = std::thread::available_parallelism().unwrap().get().min(16);
    run(&shapes::names(true), jobs, 1, false, "subset");
}

#[test]
#[ignore = "opt-in exhaustive sweep; just crash-sweep"]
fn full_sweep() {
    let selected =
        std::env::var("PLAK_CRASH_SWEEP_SHAPES").unwrap_or_else(|_| shapes::names(false).join(","));
    let names = selected.split(',').collect::<Vec<_>>();
    assert!(
        names.iter().all(|name| shapes::names(false).contains(name)),
        "unknown shape selection"
    );
    let number = |key, default| {
        std::env::var(key).map_or(default, |value| {
            value.parse::<usize>().expect("positive integer")
        })
    };
    let jobs = number("PLAK_CRASH_SWEEP_JOBS", 4);
    let repeat = number("PLAK_CRASH_SWEEP_REPEAT", 1);
    let strict = std::env::var("PLAK_CRASH_SWEEP_STRICT").is_ok_and(|value| value == "1");
    run(&names, jobs, repeat, strict, "full");
}

fn run(names: &[&str], jobs: usize, repeat: usize, strict: bool, run: &str) {
    assert!(jobs > 0 && repeat > 0);
    let table = table::Table::new(run);
    let rows = Mutex::new(Vec::<Value>::new());
    for iteration in 1..=repeat {
        for shape in names {
            let (seed, session) = driver::setup(shape);
            let baseline = summary::read_converged(&seed);
            let pilot = match std::panic::catch_unwind(|| driver::pilot(&seed, &session, shape)) {
                Ok(pilot) => pilot,
                Err(error) => {
                    rows.lock().unwrap().push(json!({"shape": shape, "repeat": iteration, "cut": "pilot", "store": "pilot", "store_ordinal": 0, "commit": "pilot", "outcome": "DIVERGENT", "invariants": false, "finding": panic_text(&error)}));
                    table.write(&rows.lock().unwrap());
                    continue;
                }
            };
            let next = AtomicUsize::new(1);
            std::thread::scope(|scope| {
                for _ in 0..jobs.min(pilot.len()) {
                    scope.spawn(|| loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        if k > pilot.len() { break; }
                        let mut row = match std::panic::catch_unwind(|| driver::cut(&seed, &session, shape, &baseline, &pilot, &pilot[k - 1])) {
                            Ok(row) => row,
                            Err(error) => json!({"outcome": "UNCLASSIFIED", "invariants": false, "finding": panic_text(&error)}),
                        };
                        row["shape"] = json!(shape);
                        row["repeat"] = json!(iteration);
                        row["cut"] = json!(format!("{}:{}", pilot[k - 1].store, pilot[k - 1].store_ordinal));
                        row["store"] = json!(pilot[k - 1].store);
                        row["store_ordinal"] = json!(pilot[k - 1].store_ordinal);
                        row["commit"] = json!(pilot[k - 1].signature());
                        let mut rows = rows.lock().unwrap();
                        rows.push(row);
                        rows.sort_by_key(|row| (row["shape"].as_str().unwrap().to_owned(), row["repeat"].as_u64().unwrap(), row["store"].as_str().unwrap().to_owned(), row["store_ordinal"].as_u64().unwrap()));
                        table.write(&rows);
                    });
                }
            });
        }
    }
    let rows = rows.into_inner().unwrap();
    table.write(&rows);
    eprintln!(
        "crash sweep table: {}",
        table.directory.join("table.md").display()
    );
    let failures = rows
        .iter()
        .filter(|row| row["invariants"] != true)
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "crash-sweep findings: {failures:?}");
    if run == "subset" {
        coverage::submission_view_cuts(&rows);
    }
    if strict {
        assert!(
            rows.iter().all(strict_accepts),
            "strict sweep rejects recovery refusals"
        );
    }
}

fn strict_accepts(row: &Value) -> bool {
    !row["outcome"].as_str().unwrap().starts_with("REFUSED")
}

#[test]
fn strict_rejects_known_refusals() {
    assert!(!strict_accepts(&json!({"outcome": "REFUSED{known}"})));
    assert!(strict_accepts(&json!({"outcome": "COMPLETED_ONCE"})));
}

fn panic_text(error: &Box<dyn std::any::Any + Send>) -> String {
    error
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            error
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
        })
        .unwrap_or_else(|| "non-text harness panic".to_owned())
}
