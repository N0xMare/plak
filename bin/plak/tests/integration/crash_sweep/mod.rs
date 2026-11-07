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
mod round_gates;
mod shapes;
mod summary;
mod table;
mod trace;
mod web_face;
mod workflow_recovery;

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn durability_subset() {
    let jobs = std::thread::available_parallelism().unwrap().get().min(16);
    run(&shapes::names(true), jobs, 1, false, "subset");
}

#[test]
fn durability_ledger_retirement_closed_cells() {
    retirement_cuts("ledger_retirement_closed_cells");
}

#[test]
fn durability_ledger_retirement_live_child_process() {
    retirement_cuts("ledger_retirement_live_child_process");
}

#[test]
fn durability_ledger_retirement_yielded_cell() {
    retirement_cuts("ledger_retirement_yielded_cell");
}

#[test]
fn durability_ledger_retirement_interrupted_batch() {
    retirement_cuts("ledger_retirement_interrupted_batch");
}

fn retirement_cuts(shape: &str) {
    run(&[shape], 1, 1, true, "retirement");
}

#[test]
fn durability_ledger_retirement_pilots() {
    for shape in [
        "ledger_retirement_live_child_process",
        "ledger_retirement_yielded_cell",
    ] {
        let (seed, session) = driver::setup(shape);
        driver::pilot(&seed, &session, shape);
    }
}

#[test]
fn durability_ledger_retirement_early_relaunches() {
    for shape in [
        "ledger_retirement_live_child_process",
        "ledger_retirement_yielded_cell",
    ] {
        let (seed, session) = driver::setup(shape);
        let baseline = summary::read_converged(&seed);
        let pilot = driver::pilot(&seed, &session, shape);
        let target = pilot
            .iter()
            .find(|commit| commit.store == "product")
            .unwrap();
        let row = driver::cut(&seed, &session, shape, &baseline, &pilot, target);
        assert_eq!(row["invariants"], true, "{shape}: {row}");
    }
}

/// Every shape the strict sweep cuts must replay its twin pilots to identical
/// per-store commit sequences, or its cuts are not comparable. The subset's
/// shapes prove it inside `durability_subset`; this proves it for the rest
/// without cutting, so a change that reorders commits fails in this lane.
#[test]
fn durability_twin_pilots() {
    let subset = shapes::names(true);
    let names = shapes::names(false)
        .into_iter()
        .filter(|name| !subset.contains(name))
        .collect::<Vec<_>>();
    let jobs = std::thread::available_parallelism()
        .unwrap()
        .get()
        .min(16)
        .min(names.len());
    let next = AtomicUsize::new(0);
    let divergent = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                while let Some(shape) = names.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let piloted = std::panic::catch_unwind(|| {
                        let (seed, session) = driver::setup(shape);
                        driver::pilot(&seed, &session, shape)
                    });
                    if let Err(error) = piloted {
                        divergent
                            .lock()
                            .unwrap()
                            .push(format!("{shape}: {}", panic_text(&error)));
                    }
                }
            });
        }
    });
    let divergent = divergent.into_inner().unwrap();
    assert!(divergent.is_empty(), "twin pilots refused: {divergent:#?}");
}

#[test]
#[ignore = "opt-in exhaustive sweep; just dev crash-sweep"]
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

/// How often a running sweep refreshes its progress tables.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// One shape's prepared run: its seed home, session, converged baseline and
/// twin-pilot commit list, shared by every cut of that shape. The seed's home
/// is removed when its last cut drops it.
struct Prepared<'a> {
    shape: &'a str,
    iteration: usize,
    seed: super::offline_support::Fixture,
    session: String,
    baseline: summary::Summary,
    pilot: Vec<trace::Commit>,
}

/// One unit of sweep work: a shape's setup and twin pilot, or one of its cuts.
enum Task<'a> {
    Prepare {
        shape: &'a str,
        iteration: usize,
    },
    Cut {
        prepared: Arc<Prepared<'a>>,
        index: usize,
    },
}

/// The work every worker takes from: tasks not started, how many run, and a
/// setup panic that ends the sweep.
struct Work<'a> {
    tasks: VecDeque<Task<'a>>,
    running: usize,
    aborted: Option<Box<dyn std::any::Any + Send>>,
}

/// Runs every shape's setup, twin pilot and cuts on one pool of `jobs`
/// workers. A shape's cuts are queued ahead of the setups still waiting, so a
/// prepared seed is cut and released before more shapes are prepared, and no
/// worker sits idle behind a serial setup or pilot. Rows are written sorted
/// by shape, repeat, store and ordinal, so the tables do not depend on the
/// order work finished in. `timings.jsonl` beside them records each task's
/// start and duration.
fn run(names: &[&str], jobs: usize, repeat: usize, strict: bool, run: &str) {
    assert!(jobs > 0 && repeat > 0);
    let table = table::Table::new(run);
    let rows = Mutex::new(Vec::<Value>::new());
    // The progress copy of the tables is rewritten at most once an interval,
    // by whichever worker finds it due, outside the rows lock: rewriting the
    // whole multi-megabyte `sweep.jsonl` after every row made each finished
    // cut wait on the last one's write.
    let written = Mutex::new(std::time::Instant::now());
    let record = |row: Value| {
        rows.lock().unwrap().push(row);
        if let Ok(mut written) = written.try_lock()
            && written.elapsed() >= PROGRESS_INTERVAL
        {
            let mut snapshot = rows.lock().unwrap().clone();
            snapshot.sort_by_key(row_key);
            table.write(&snapshot);
            *written = std::time::Instant::now();
        }
    };
    let work = Mutex::new(Work {
        tasks: (1..=repeat)
            .flat_map(|iteration| {
                names
                    .iter()
                    .map(move |&shape| Task::Prepare { shape, iteration })
            })
            .collect(),
        running: 0,
        aborted: None,
    });
    let changed = Condvar::new();
    let started = std::time::Instant::now();
    let timings = Mutex::new(std::fs::File::create(table.directory.join("timings.jsonl")).unwrap());
    let timed =
        |kind: &str, shape: &str, iteration: usize, cut: Value, begun: std::time::Instant| {
            use std::io::Write as _;
            let line = json!({
                "kind": kind,
                "shape": shape,
                "repeat": iteration,
                "cut": cut,
                "start_ms": begun.duration_since(started).as_millis(),
                "elapsed_ms": begun.elapsed().as_millis(),
            });
            writeln!(timings.lock().unwrap(), "{line}").unwrap();
        };
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| work_on(&work, &changed, &record, &timed));
        }
    });
    if let Some(error) = work.into_inner().unwrap().aborted {
        std::panic::resume_unwind(error);
    }
    let mut rows = rows.into_inner().unwrap();
    rows.sort_by_key(row_key);
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

/// One worker: takes tasks until none is left and none still runs (one
/// may still queue cuts), or until a setup failed.
fn work_on<'a>(
    work: &Mutex<Work<'a>>,
    changed: &Condvar,
    record: &(dyn Fn(Value) + Sync),
    timed: &(dyn Fn(&str, &str, usize, Value, std::time::Instant) + Sync),
) {
    loop {
        let task = {
            let mut work = work.lock().unwrap();
            loop {
                if work.aborted.is_some() {
                    return;
                }
                if let Some(task) = work.tasks.pop_front() {
                    work.running += 1;
                    break task;
                }
                if work.running == 0 {
                    return;
                }
                work = changed.wait(work).unwrap();
            }
        };
        let begun = std::time::Instant::now();
        let (follow, aborted) = match task {
            Task::Prepare { shape, iteration } => {
                let prepared = prepare(shape, iteration, record);
                timed("prepare", shape, iteration, Value::Null, begun);
                match prepared {
                    Ok(cuts) => (cuts, None),
                    Err(error) => (Vec::new(), Some(error)),
                }
            }
            Task::Cut { prepared, index } => {
                let row = cut_row(&prepared, index);
                timed(
                    "cut",
                    prepared.shape,
                    prepared.iteration,
                    row["cut"].clone(),
                    begun,
                );
                record(row);
                (Vec::new(), None)
            }
        };
        let mut work = work.lock().unwrap();
        for task in follow.into_iter().rev() {
            work.tasks.push_front(task);
        }
        if aborted.is_some() && work.aborted.is_none() {
            work.aborted = aborted;
        }
        work.running -= 1;
        changed.notify_all();
    }
}

/// Sets up one shape and runs its twin pilot, returning its cuts. A divergent
/// or failed pilot is recorded as the shape's one row; a failed setup ends
/// the sweep, as a setup the harness cannot build is not a cut finding.
fn prepare<'a>(
    shape: &'a str,
    iteration: usize,
    record: &(dyn Fn(Value) + Sync),
) -> Result<Vec<Task<'a>>, Box<dyn std::any::Any + Send>> {
    let (seed, session) = std::panic::catch_unwind(|| driver::setup(shape))?;
    let baseline = summary::read_converged(&seed);
    let pilot = match std::panic::catch_unwind(|| driver::pilot(&seed, &session, shape)) {
        Ok(pilot) => pilot,
        Err(error) => {
            record(
                json!({"shape": shape, "repeat": iteration, "cut": "pilot", "store": "pilot", "store_ordinal": 0, "commit": "pilot", "outcome": "DIVERGENT", "invariants": false, "finding": panic_text(&error)}),
            );
            return Ok(Vec::new());
        }
    };
    let prepared = Arc::new(Prepared {
        shape,
        iteration,
        seed,
        session,
        baseline,
        pilot,
    });
    Ok((0..prepared.pilot.len())
        .map(|index| Task::Cut {
            prepared: Arc::clone(&prepared),
            index,
        })
        .collect())
}

/// One cut of a prepared shape, as its table row.
fn cut_row(prepared: &Prepared<'_>, index: usize) -> Value {
    let target = &prepared.pilot[index];
    let mut row = match std::panic::catch_unwind(|| {
        driver::cut(
            &prepared.seed,
            &prepared.session,
            prepared.shape,
            &prepared.baseline,
            &prepared.pilot,
            target,
        )
    }) {
        Ok(row) => row,
        Err(error) => {
            json!({"outcome": "UNCLASSIFIED", "invariants": false, "finding": panic_text(&error)})
        }
    };
    row["shape"] = json!(prepared.shape);
    row["repeat"] = json!(prepared.iteration);
    row["cut"] = json!(format!("{}:{}", target.store, target.store_ordinal));
    row["store"] = json!(target.store);
    row["store_ordinal"] = json!(target.store_ordinal);
    row["commit"] = json!(target.signature());
    row
}

fn row_key(row: &Value) -> (String, u64, String, u64) {
    (
        row["shape"].as_str().unwrap().to_owned(),
        row["repeat"].as_u64().unwrap(),
        row["store"].as_str().unwrap().to_owned(),
        row["store_ordinal"].as_u64().unwrap(),
    )
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
