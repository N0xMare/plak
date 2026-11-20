//! Which select arm ended the pass that took each queued follow-up, so a
//! test can tell an intent wake from the coalescing tick. Test builds only.

use std::sync::Mutex;

static TAKEN: Mutex<Vec<(String, &'static str)>> = Mutex::new(Vec::new());

/// The pass after the drive's last wake, on `arm`, admitted the follow-up
/// `text`. The drive itself carries the arm, whichever thread polls it.
pub(super) fn took(text: &str, arm: &'static str) {
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
