//! Test seams for a stop's crash cuts, the stop answer's wake, and the idle
//! release's read-before-receive. Armed by the test that needs them and
//! cleared when that test drops its guard.
#![allow(clippy::unwrap_used, reason = "test seams")]

use std::sync::{Condvar, Mutex};
use std::time::Duration;

struct Seams {
    stop_session: Option<String>,
    stop_at: Option<String>,
    /// `(session, delay)`. A delay armed for another session is ignored.
    notice_delay: Option<(String, Duration)>,
    idle_session: Option<String>,
    idle_entered: bool,
    idle_go: bool,
    /// The session whose child retirement the stop treats as timed out.
    force_retirement_timeout: Option<String>,
}

impl Seams {
    const fn new() -> Self {
        Self {
            stop_session: None,
            stop_at: None,
            notice_delay: None,
            idle_session: None,
            idle_entered: false,
            idle_go: false,
            force_retirement_timeout: None,
        }
    }
}

static SEAMS: Mutex<Seams> = Mutex::new(Seams::new());
static IDLE: Condvar = Condvar::new();

type IdleReadCut = Box<dyn FnOnce() + Send>;
static IDLE_READ_CUTS: std::sync::LazyLock<Mutex<std::collections::BTreeMap<String, IdleReadCut>>> =
    std::sync::LazyLock::new(Mutex::default);

pub(crate) fn arm_after_idle_read(session: &str, cut: IdleReadCut) {
    IDLE_READ_CUTS
        .lock()
        .unwrap()
        .insert(session.to_owned(), cut);
}

pub(crate) fn after_idle_read(session: &str) {
    let cut = IDLE_READ_CUTS.lock().unwrap().remove(session);
    if let Some(cut) = cut {
        cut();
    }
}

fn lock() -> std::sync::MutexGuard<'static, Seams> {
    SEAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn disarm() {
    *lock() = Seams::new();
    IDLE.notify_all();
}

pub(crate) struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        disarm();
    }
}

pub(crate) fn arm_stop_cut(session: &str, at: &str) {
    let mut seams = lock();
    seams.stop_session = Some(session.to_owned());
    seams.stop_at = Some(at.to_owned());
}

/// Whether this stop should die at `at`, as a crash between writes would.
pub(crate) fn stop_cut(session: &str, at: &str) -> bool {
    let seams = lock();
    seams.stop_session.as_deref() == Some(session) && seams.stop_at.as_deref() == Some(at)
}

pub(crate) fn arm_notice_delay(session: &str, delay: Duration) {
    lock().notice_delay = Some((session.to_owned(), delay));
}

pub(crate) fn notice_delay(session: &str) -> Option<Duration> {
    let seams = lock();
    seams
        .notice_delay
        .as_ref()
        .and_then(|(id, delay)| (*id == session).then_some(*delay))
}

pub(crate) fn arm_idle_release(session: &str) {
    let mut seams = lock();
    seams.idle_session = Some(session.to_owned());
    seams.idle_entered = false;
    seams.idle_go = false;
}

/// Blocks the actor at the idle-release decision until the test says go.
pub(crate) fn before_idle_release(session: &str) {
    let mut seams = lock();
    if seams.idle_session.as_deref() != Some(session) {
        return;
    }
    seams.idle_entered = true;
    IDLE.notify_all();
    while !seams.idle_go {
        seams = IDLE
            .wait(seams)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

pub(crate) fn wait_until_idle_seam() {
    let mut seams = lock();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !seams.idle_entered {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "the actor never reached the idle-release decision"
        );
        let (guard, wait) = IDLE
            .wait_timeout(seams, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        seams = guard;
        assert!(
            !wait.timed_out() || seams.idle_entered,
            "the actor never reached the idle-release decision"
        );
    }
}

pub(crate) fn release_idle_seam() {
    lock().idle_go = true;
    IDLE.notify_all();
}

pub(crate) fn force_retirement_timeout(session: &str) {
    lock().force_retirement_timeout = Some(session.to_owned());
}

pub(crate) fn retirement_timeout_forced(session: &str) -> bool {
    lock().force_retirement_timeout.as_deref() == Some(session)
}
