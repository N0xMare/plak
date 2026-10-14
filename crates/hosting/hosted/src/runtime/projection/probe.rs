//! Test-only entry and view-writer accounting, isolated by product session id.

use std::cell::RefCell;
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

thread_local! {
    static STORE_CALL: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(super) fn store_call<T>(id: &str, call: impl FnOnce() -> T) -> T {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            STORE_CALL.with(|current| {
                current.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(STORE_CALL.with(|current| current.replace(Some(id.to_owned()))));
    call()
}

#[derive(Clone, Copy, Default)]
pub(in crate::runtime) struct Counts {
    pub(super) open: usize,
    pub(super) live: usize,
    pub(in crate::runtime) release: usize,
    pub(super) other: usize,
    pub(in crate::runtime) writes: usize,
    active: usize,
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
            STORE_CALL.with(|current| current.borrow().as_deref() == Some(id)),
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
}
