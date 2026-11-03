//! Test-only entry and view-writer accounting, isolated by product session id.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy)]
pub(in crate::runtime) enum Caller {
    Open,
    Live,
    Release,
}

tokio::task_local! {
    pub(in crate::runtime) static CALLER: Caller;
}

/// Product ids with a projector store call in flight, and how many: a view
/// write is legal only inside one, whichever thread runs it.
fn store_calls() -> &'static Mutex<BTreeMap<String, usize>> {
    static CALLS: OnceLock<Mutex<BTreeMap<String, usize>>> = OnceLock::new();
    CALLS.get_or_init(Mutex::default)
}

pub(super) fn store_call<T>(id: &str, call: impl FnOnce() -> T) -> T {
    struct Leave(String);
    impl Drop for Leave {
        fn drop(&mut self) {
            let mut calls = store_calls().lock().unwrap();
            if let Some(count) = calls.get_mut(&self.0) {
                *count -= 1;
                if *count == 0 {
                    calls.remove(&self.0);
                }
            }
        }
    }
    *store_calls()
        .lock()
        .unwrap()
        .entry(id.to_owned())
        .or_default() += 1;
    let _leave = Leave(id.to_owned());
    call()
}

#[derive(Clone, Copy, Default)]
pub(in crate::runtime) struct Counts {
    pub(super) open: usize,
    pub(super) live: usize,
    pub(in crate::runtime) release: usize,
    pub(super) other: usize,
    pub(in crate::runtime) writes: usize,
    pub(in crate::runtime) live_view_reads: usize,
    active: usize,
}

impl Counts {
    pub(in crate::runtime) const fn open(&self) -> usize {
        self.open
    }
}

fn counts() -> &'static Mutex<BTreeMap<String, Counts>> {
    static COUNTS: OnceLock<Mutex<BTreeMap<String, Counts>>> = OnceLock::new();
    COUNTS.get_or_init(Mutex::default)
}

pub(in crate::runtime) fn track(id: &str) {
    counts()
        .lock()
        .unwrap()
        .insert(id.to_owned(), Counts::default());
}

pub(in crate::runtime) fn take(id: &str) -> Counts {
    let result = counts().lock().unwrap().remove(id).unwrap();
    assert_eq!(result.active, 0, "projector still running");
    result
}

pub(super) struct Entry(String);

pub(super) fn enter(id: &str) -> Entry {
    if let Some(count) = counts().lock().unwrap().get_mut(id) {
        count.active += 1;
        match CALLER.try_with(|caller| *caller) {
            Ok(Caller::Open) => count.open += 1,
            Ok(Caller::Live) => count.live += 1,
            Ok(Caller::Release) => count.release += 1,
            Err(_) => count.other += 1,
        }
    }
    Entry(id.to_owned())
}

pub(super) fn view_read(id: &str) {
    if matches!(CALLER.try_with(|caller| *caller), Ok(Caller::Live))
        && let Some(count) = counts().lock().unwrap().get_mut(id)
    {
        count.live_view_reads += 1;
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(count) = counts().lock().unwrap().get_mut(&self.0) {
            count.active -= 1;
        }
    }
}

pub(crate) fn view_write(id: &str) {
    let tracked = counts().lock().unwrap().contains_key(id);
    if tracked {
        assert!(
            store_calls().lock().unwrap().contains_key(id),
            "view writer bypassed the common projector's store call",
        );
    }
    if let Some(count) = counts().lock().unwrap().get_mut(id) {
        assert!(
            count.active > 0,
            "view writer bypassed the common projector entry"
        );
        count.writes += 1;
    }
    let hook = write_hooks().lock().unwrap().remove(id);
    if let Some(hook) = hook {
        hook();
    }
}

type WriteHook = Box<dyn FnOnce() + Send>;

fn write_hooks() -> &'static Mutex<BTreeMap<String, WriteHook>> {
    static HOOKS: OnceLock<Mutex<BTreeMap<String, WriteHook>>> = OnceLock::new();
    HOOKS.get_or_init(Mutex::default)
}

pub(in crate::runtime) fn after_next_write(id: String, hook: impl FnOnce() + Send + 'static) {
    write_hooks().lock().unwrap().insert(id, Box::new(hook));
}
