use std::os::fd::{AsFd as _, OwnedFd};
use std::time::{Duration, Instant};

fn input() -> (OwnedFd, OwnedFd) {
    let (read, write) = std::io::pipe().expect("input pipe");
    (read.into(), write.into())
}

/// Wakes written before the wait return it at once and are consumed
/// together, so the next bounded wait runs out its bound.
#[test]
fn pending_wakes_return_the_wait_and_are_consumed() {
    let pipe = super::open_pipe().expect("wake pipe");
    let (input, _writer) = input();
    pipe.ring();
    pipe.ring();
    let started = Instant::now();
    pipe.wait_on(Some(input.as_fd()), None).expect("wait");
    assert!(started.elapsed() < Duration::from_secs(5));
    let started = Instant::now();
    pipe.wait_on(Some(input.as_fd()), Some(Duration::from_millis(50)))
        .expect("wait");
    assert!(started.elapsed() >= Duration::from_millis(50));
}

/// A wake from another thread ends an unbounded wait.
#[test]
fn a_wake_from_another_thread_ends_an_unbounded_wait() {
    let pipe = std::sync::Arc::new(super::open_pipe().expect("wake pipe"));
    let (input, _writer) = input();
    let ringer = std::sync::Arc::clone(&pipe);
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        ringer.ring();
    });
    pipe.wait_on(Some(input.as_fd()), None).expect("wait");
    thread.join().expect("ringer");
}

/// Readable input ends the wait without a wake, and says so; a wait
/// that does not watch the input times out over the same bytes.
#[test]
fn readable_input_ends_a_watching_wait_only() {
    let pipe = super::open_pipe().expect("wake pipe");
    let (input, writer) = input();
    rustix::io::write(&writer, b"x").expect("input byte");
    assert_eq!(
        pipe.wait_on(Some(input.as_fd()), None).expect("wait"),
        super::Input::Readable
    );
    let started = Instant::now();
    assert_eq!(
        pipe.wait_on(None, Some(Duration::from_millis(30)))
            .expect("wait"),
        super::Input::Quiet
    );
    assert!(started.elapsed() >= Duration::from_millis(30));
}

/// A readable wake whose drain takes no event leaves bytes crossterm
/// reads only on the next arrival: the loop stops polling them and looks
/// again on the bounded tick until an event comes through, so stranded
/// input never spins the loop and never parks it deaf.
#[test]
fn stranded_input_is_looked_at_on_a_tick_until_it_drains() {
    use super::{Input, InputMode, InputWatch, STRANDED_INPUT_TICK};
    let tick = Some(STRANDED_INPUT_TICK);
    let mut watch = InputWatch::default();
    assert_eq!(watch.next_wait(None), (None, InputMode::Watch));
    // Bytes that yield no event: stop waiting on them, look on a tick.
    watch.observe(Input::Readable, 0);
    assert_eq!(watch.next_wait(None), (tick, InputMode::Look));
    let soon = Duration::from_millis(5);
    assert_eq!(watch.next_wait(Some(soon)), (Some(soon), InputMode::Look));
    watch.observe(Input::Readable, 0);
    assert_eq!(watch.next_wait(None), (tick, InputMode::Look));
    // An arrival lets crossterm read: watch again, one look a tick on.
    watch.observe(Input::Readable, 2);
    assert_eq!(watch.next_wait(None), (tick, InputMode::Watch));
    // A look that finds the kernel empty ends the ticking.
    watch.observe(Input::Readable, 0);
    assert_eq!(watch.next_wait(None), (tick, InputMode::Look));
    watch.observe(Input::Quiet, 0);
    assert_eq!(watch.next_wait(None), (None, InputMode::Watch));
    // An unwatchable or hung-up descriptor is ticked for good.
    watch.observe(Input::Unpollable, 0);
    assert_eq!(watch.next_wait(None), (tick, InputMode::Ignore));
    let mut hung = InputWatch::default();
    hung.observe(Input::HungUp, 0);
    assert_eq!(hung.next_wait(None), (tick, InputMode::Ignore));
}

/// Crossterm's zero-timeout poll can stop at an internal reply it set
/// aside (a cursor-position report, a keyboard-flags answer) while real
/// events stay parsed behind it, and the drain then counts some events
/// and stops. A readable wake therefore earns one more look a tick
/// later, while still watching the descriptor, and then parks.
#[test]
fn every_readable_wake_earns_one_more_look() {
    use super::{Input, InputMode, InputWatch, STRANDED_INPUT_TICK};
    let mut watch = InputWatch::default();
    watch.observe(Input::Readable, 1);
    assert_eq!(
        watch.next_wait(None),
        (Some(STRANDED_INPUT_TICK), InputMode::Watch)
    );
    watch.observe(Input::Quiet, 1);
    assert_eq!(watch.next_wait(None), (None, InputMode::Watch));
}

/// A descriptor whose other end is gone reports a hang-up, not
/// readable input crossterm would spin reading.
#[cfg(target_os = "linux")]
#[test]
fn a_closed_input_reports_a_hang_up() {
    let pipe = super::open_pipe().expect("wake pipe");
    let (input, writer) = input();
    drop(writer);
    assert_eq!(
        pipe.wait_on(Some(input.as_fd()), None).expect("wait"),
        super::Input::HungUp
    );
    assert_eq!(
        super::look(input.as_fd()).expect("look"),
        super::Input::HungUp
    );
}
