//! Which select arm ended the pass that took each queued follow-up, so a
//! test can tell an intent wake from the coalescing tick. Test builds only.

use std::sync::Mutex;

std::thread_local! {
    static LAST_WAKE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("none") };
}

static TAKEN: Mutex<Vec<(String, &'static str)>> = Mutex::new(Vec::new());

/// The busy select ended on `arm`.
pub(super) fn woke(arm: &'static str) {
    LAST_WAKE.with(|last| last.set(arm));
}

/// The pass after the last wake admitted the follow-up `text`.
pub(super) fn took(text: &str) {
    let arm = LAST_WAKE.with(std::cell::Cell::get);
    TAKEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((text.to_owned(), arm));
}

/// The wake that preceded the admission of `text`, if it was taken.
pub(in crate::runtime) fn wake_before(text: &str) -> Option<&'static str> {
    TAKEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(taken, _)| taken == text)
        .map(|(_, arm)| *arm)
}
