//! Repository-only SQLite crash cuts. See spec 05, "Commit trace contract".
//!
//! No hooks or files are installed unless `PLAK_TEST_COMMIT_TRACE` is set.
//! Commits before installation are untraced, including runtime schema/domain
//! admission. Traced writes must be stepped to completion. A write committed by
//! reset or finalize (`RETURNING` through `query` or `query_row`) loses its WAL
//! callback and is untraceable. SQL `RETURNING` is therefore forbidden in traced
//! stores by the structure checker (spec 05 section 2.6).
//! `VACUUM` is unsupported under trace.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use nix::sys::signal::{Signal, raise};
use rusqlite::Connection;
use rusqlite::hooks::{CheckpointMode, Wal};

/// JSONL destination; one fresh file per traced process.
pub const TRACE_ENV: &str = "PLAK_TEST_COMMIT_TRACE";
/// Optional `runtime|product|tree:<positive ordinal>` target for `SIGKILL`.
pub const KILL_ENV: &str = "PLAK_TEST_COMMIT_KILL";

const SEMAPHORE_TIMEOUT: Duration = Duration::from_secs(5);
static TRACE: OnceLock<Arc<Trace>> = OnceLock::new();
static CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static LABEL: Cell<&'static str> = const { Cell::new("unlabelled") };
    // rusqlite's WAL callback accepts a fn pointer, not a captured closure.
    // SQLite runs commit and WAL callbacks synchronously on the caller's thread.
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
}

/// Restores the previous store-method label when the dispatch closure returns.
#[must_use]
pub struct LabelGuard {
    previous: &'static str,
    _thread: PhantomData<Rc<()>>,
}

impl Drop for LabelGuard {
    fn drop(&mut self) {
        LABEL.set(self.previous);
    }
}

/// Labels commits on this thread for the guard's lifetime (including nesting).
pub fn label(name: &'static str) -> LabelGuard {
    LabelGuard {
        previous: LABEL.replace(name),
        _thread: PhantomData,
    }
}

/// Releases this thread's frameless commit claim at a completed operation boundary.
///
/// Call only outside `SQLite`, after writes have been fully stepped and transactions
/// have committed or rolled back. Framed commits already consumed their claim in
/// the WAL callback. Reset/finalize-completed writes are untraceable and forbidden
/// by the traced-store SQL structure pin (spec 05 section 2.6).
pub fn settle() {
    if let Some(pending) = PENDING.with(|pending| pending.borrow_mut().take()) {
        lock(&pending.context.tables).clear();
        pending.context.trace.release(pending.context.id);
    }
}

/// Settles at an operation boundary even if the operation unwinds.
/// Declare before the operation's transactions so rollback precedes settlement.
#[must_use]
pub struct SettleGuard;

impl Drop for SettleGuard {
    fn drop(&mut self) {
        settle();
    }
}

struct Trace {
    gate: Mutex<Gate>,
    released: Condvar,
}

struct Gate {
    owner: Option<u64>,
    ordinal: u64,
    store_indices: BTreeMap<&'static str, u64>,
    store_ordinals: BTreeMap<&'static str, u64>,
    file: File,
    kill: Option<(&'static str, u64)>,
}

struct Context {
    id: u64,
    store: &'static str,
    tables: Mutex<BTreeSet<String>>,
    trace: Arc<Trace>,
}

struct Pending {
    context: Arc<Context>,
    label: &'static str,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|_| diverged("poisoned trace lock"))
}

fn diverged(reason: &str) -> ! {
    eprintln!("DIVERGENT commit-trace: {reason}");
    std::process::exit(86);
}

impl Trace {
    fn acquire(&self, id: u64) {
        let gate = lock(&self.gate);
        let (mut gate, timeout) = self
            .released
            .wait_timeout_while(gate, SEMAPHORE_TIMEOUT, |gate| {
                gate.owner.is_some_and(|owner| owner != id)
            })
            .unwrap_or_else(|_| diverged("poisoned trace semaphore"));
        if timeout.timed_out() && gate.owner.is_some_and(|owner| owner != id) {
            diverged("commit semaphore exceeded 5 seconds");
        }
        gate.owner = Some(id);
    }

    fn release(&self, id: u64) {
        let mut gate = lock(&self.gate);
        if gate.owner == Some(id) {
            gate.owner = None;
            self.released.notify_one();
        }
    }
}

/// Installs the shared tracer on a writable WAL connection.
///
/// Returns false without changing the connection when the trace env is absent.
/// `store` is one of `runtime`, `product`, `tree`. Call after WAL configuration;
/// installing any later WAL hook or autocheckpoint pragma would replace this hook.
///
/// # Errors
/// Returns `SQLite` hook registration errors. Invalid trace controls or trace I/O
/// failure terminate the traced process with a divergence report (exit 86).
pub fn install(connection: &Connection, store: &'static str) -> rusqlite::Result<bool> {
    let Some(path) = std::env::var_os(TRACE_ENV) else {
        return Ok(false);
    };
    let trace = TRACE.get_or_init(|| {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|_| diverged("trace destination must be a new writable file"));
        let kill = std::env::var_os(KILL_ENV).map(|value| parse_kill(value.to_str()));
        Arc::new(Trace {
            gate: Mutex::new(Gate {
                owner: None,
                ordinal: 0,
                store_indices: BTreeMap::new(),
                store_ordinals: BTreeMap::new(),
                file,
                kill,
            }),
            released: Condvar::new(),
        })
    });
    let context = Arc::new(Context {
        id: CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
        store,
        tables: Mutex::new(BTreeSet::new()),
        trace: trace.clone(),
    });
    let updates = context.clone();
    connection.update_hook(Some(move |_, database: &str, table: &str, _| {
        if database == "main" {
            lock(&updates.tables).insert(table.to_owned());
        }
    }))?;
    let commits = context.clone();
    connection.commit_hook(Some(move || {
        // A previous sqlite3_step returned without a WAL callback. A retry
        // after BUSY must retain this transaction's observed table names.
        if let Some(pending) = PENDING.with(|pending| pending.borrow_mut().take()) {
            if pending.context.id != commits.id {
                lock(&pending.context.tables).clear();
            }
            pending.context.trace.release(pending.context.id);
        }
        commits.trace.acquire(commits.id);
        PENDING.with(|pending| {
            *pending.borrow_mut() = Some(Pending {
                context: commits.clone(),
                label: LABEL.get(),
            });
        });
        false
    }))?;
    connection.rollback_hook(Some(move || rolled_back(&context)))?;
    connection.wal_hook(Some(|wal, pages| {
        committed(wal, pages);
        Ok(())
    }));
    Ok(true)
}

fn parse_kill(value: Option<&str>) -> (&'static str, u64) {
    fn invalid() -> ! {
        diverged("kill target must be runtime|product|tree:<positive ordinal>");
    }
    let (store, ordinal) = value
        .and_then(|value| value.split_once(':'))
        .unwrap_or_else(|| invalid());
    let store = match store {
        "runtime" => "runtime",
        "product" => "product",
        "tree" => "tree",
        _ => invalid(),
    };
    if ordinal.is_empty() || !ordinal.bytes().all(|byte| byte.is_ascii_digit()) {
        invalid();
    }
    let ordinal = ordinal
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or_else(|| invalid());
    (store, ordinal)
}

fn rolled_back(context: &Context) {
    lock(&context.tables).clear();
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.as_ref().is_some_and(|p| p.context.id == context.id) {
            *pending = None;
        }
    });
    context.trace.release(context.id);
}

/// A commit that writes only projector-derived rows: the live projector runs
/// on a background task whose passes coalesce, so their count and placement
/// vary between twin runs. Like lease renewals they are traced but neither
/// ordered nor killable; the next open re-derives them from the feed.
fn projector_only(store: &str, tables: &BTreeSet<String>) -> bool {
    let derived: &[&str] = match store {
        "product" => &[
            "projector_cursors",
            "session_views",
            "transcript_rows",
            "session_call_views",
            "session_open_turn_views",
            "session_process_views",
        ],
        _ => return false,
    };
    !tables.is_empty() && tables.iter().all(|table| derived.contains(&table.as_str()))
}

fn committed(wal: &Wal, pages: i32) {
    let pending = PENDING
        .with(|pending| pending.borrow_mut().take())
        .unwrap_or_else(|| diverged("WAL callback without commit owner"));
    let context = pending.context;
    let tables = std::mem::take(&mut *lock(&context.tables));
    let indexed = (context.store != "runtime" || !matches!(pending.label, "renew" | "revive"))
        && !projector_only(context.store, &tables);
    {
        let mut gate = lock(&context.trace.gate);
        if gate.owner != Some(context.id) {
            diverged("WAL callback lost commit owner");
        }
        if indexed {
            gate.ordinal += 1;
        }
        let store_index = gate.store_indices.entry(context.store).or_default();
        *store_index += 1;
        let store_index = *store_index;
        let store_ordinal = gate.store_ordinals.entry(context.store).or_default();
        if indexed {
            *store_ordinal += 1;
        }
        let store_ordinal = *store_ordinal;
        let line = serde_json::json!({
            "ordinal": gate.ordinal,
            "store": context.store,
            "store_index": store_index,
            "store_ordinal": store_ordinal,
            "label": pending.label,
            "tables": tables,
            "indexed": indexed,
        });
        if serde_json::to_writer(&mut gate.file, &line).is_err()
            || gate.file.write_all(b"\n").is_err()
            || gate.file.sync_all().is_err()
        {
            diverged("could not fsync trace line");
        }
        if indexed && gate.kill == Some((context.store, store_ordinal)) {
            if raise(Signal::SIGKILL).is_err() {
                diverged("could not raise SIGKILL");
            }
            diverged("SIGKILL returned");
        }
    }
    // Like SQLite's default autocheckpointer, PASSIVE is best-effort and its
    // result must not turn an already committed transaction into an error.
    if pages >= 1000 {
        let _ = wal.checkpoint_v2(CheckpointMode::PASSIVE);
    }
    context.trace.release(context.id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_product_views_are_unindexed_but_intent_writes_are_not() {
        let mut tables =
            BTreeSet::from(["projector_cursors".to_owned(), "session_views".to_owned()]);
        for table in [
            "session_call_views",
            "session_open_turn_views",
            "session_process_views",
        ] {
            let single = BTreeSet::from([table.to_owned()]);
            assert!(projector_only("product", &single));
            assert!(!projector_only("runtime", &single));
            tables.insert(table.to_owned());
        }
        assert!(projector_only("product", &tables));
        for table in [
            "sessions",
            "submissions",
            "workflow_requests",
            "operator_notices",
        ] {
            let mut mixed = tables.clone();
            mixed.insert(table.to_owned());
            assert!(!projector_only("product", &mixed));
        }
        assert!(!projector_only("product", &BTreeSet::new()));
    }

    #[test]
    fn rollback_releases_only_its_own_pending_commit() -> std::io::Result<()> {
        let trace = Arc::new(Trace {
            gate: Mutex::new(Gate {
                owner: None,
                ordinal: 0,
                store_indices: BTreeMap::new(),
                store_ordinals: BTreeMap::new(),
                file: tempfile::tempfile()?,
                kill: None,
            }),
            released: Condvar::new(),
        });
        let owner = Arc::new(Context {
            id: 1,
            store: "runtime",
            tables: Mutex::new(BTreeSet::from(["a".to_owned()])),
            trace: trace.clone(),
        });
        let other = Context {
            id: 2,
            store: "tree",
            tables: Mutex::new(BTreeSet::new()),
            trace: trace.clone(),
        };
        trace.acquire(owner.id);
        trace.acquire(owner.id);
        PENDING.with(|pending| {
            *pending.borrow_mut() = Some(Pending {
                context: owner.clone(),
                label: "commit",
            });
        });
        rolled_back(&other);
        assert_eq!(lock(&trace.gate).owner, Some(1));
        assert!(PENDING.with(|pending| pending.borrow().is_some()));
        rolled_back(&owner);
        assert_eq!(lock(&trace.gate).owner, None);
        assert!(PENDING.with(|pending| pending.borrow().is_none()));
        assert!(lock(&owner.tables).is_empty());
        trace.acquire(2);
        trace.release(2);
        Ok(())
    }

    #[test]
    fn labels_restore_after_nested_scope_and_unwind() {
        assert_eq!(LABEL.get(), "unlabelled");
        let outer = label("create");
        {
            let _inner = label("renew");
            assert_eq!(LABEL.get(), "renew");
        }
        assert_eq!(LABEL.get(), "create");
        let _ = std::panic::catch_unwind(|| {
            let _inner = label("commit");
            panic!("test unwind");
        });
        assert_eq!(LABEL.get(), "create");
        drop(outer);
        assert_eq!(LABEL.get(), "unlabelled");
    }

    #[test]
    fn settlement_guard_releases_frameless_claim_after_unwind() -> std::io::Result<()> {
        let trace = Arc::new(Trace {
            gate: Mutex::new(Gate {
                owner: None,
                ordinal: 0,
                store_indices: BTreeMap::new(),
                store_ordinals: BTreeMap::new(),
                file: tempfile::tempfile()?,
                kill: None,
            }),
            released: Condvar::new(),
        });
        let context = Arc::new(Context {
            id: 1,
            store: "runtime",
            tables: Mutex::new(BTreeSet::from(["a".to_owned()])),
            trace: trace.clone(),
        });
        let panic = std::panic::catch_unwind(|| {
            let _settle = SettleGuard;
            trace.acquire(context.id);
            PENDING.with(|pending| {
                *pending.borrow_mut() = Some(Pending {
                    context: context.clone(),
                    label: "commit",
                });
            });
            panic!("test operation unwind");
        });
        assert!(panic.is_err());
        assert!(PENDING.with(|pending| pending.borrow().is_none()));
        assert!(lock(&context.tables).is_empty());
        let gate = lock(&trace.gate);
        assert_eq!(gate.owner, None);
        assert_eq!(gate.ordinal, 0);
        assert!(gate.store_ordinals.is_empty());
        drop(gate);
        trace.acquire(2);
        trace.release(2);
        Ok(())
    }
}
