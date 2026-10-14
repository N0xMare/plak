//! Terminating-signal handling for every face.
//!
//! A supervisor that stops Plak sends `SIGTERM`, and a closed terminal sends
//! `SIGHUP`. Without a handler the default disposition ends the process
//! immediately, so the turn ends with empty stdout and empty stderr and the
//! operator is left with a bare exit code. A live qualification run hit exactly
//! that: the harness stopped the case, sent `SIGTERM`, and the process vanished
//! having written its request capture and nothing else. The TUI fared worse:
//! it died inside the alternate screen with raw mode, mouse reporting and
//! keyboard flags still on.
//!
//! **A dedicated thread owns the announcement.** It runs its own single-thread
//! runtime whose only job is to wait for those two signals, so it is always
//! scheduled. An earlier attempt watched the signals from a `tokio` task on the
//! face's own runtime; registering the handler suppresses the default
//! disposition process-wide, so when that runtime sat between `block_on` calls
//! the signal was neither fatal nor observed and the process hung. A thread that
//! does nothing else cannot be starved that way.
//!
//! **Each face supplies its own stop.** The two faces stop by different means:
//! the CLI raises the interrupt its observer already polls, and the JSONL face
//! sends the same quit intent its loop is blocked on. Handing the JSONL face a
//! counter nobody read is what left every signalled JSONL session unresumable —
//! the graceful stop was never requested, the watcher waited out its whole
//! deadline, and the hard exit left the branch head stale.
//!
//! The thread writes the face's terminal record and exits immediately, well
//! inside a supervisor's grace period. `SIGINT` is deliberately untouched:
//! Ctrl-C keeps its graceful cancel-and-join path and its own interrupt status.
//!
//! **The TUI quits on its own loop.** [`install_interactive`] only records the
//! signal; the event loop polls [`signalled_stops`] and quits exactly as
//! `Ctrl+Q` does, so the terminal is restored by the ordinary restoration
//! before the session is released. Only a second signal, or a loop that has
//! not exited by [`INTERACTIVE_FORCED_EXIT_DEADLINE`], makes the watcher
//! restore the terminal itself and exit. Nothing here survives the `/reload`
//! `exec`: caught signals reset to their default disposition across `exec`,
//! no signal is ignored or blocked, and the watcher thread does not outlive
//! the old image, so the new process arms its own watcher from scratch.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// How long the graceful stop may take before the process exits regardless.
///
/// A supervisor allows about two seconds before `SIGKILL`, and the graceful
/// path measures around 0.3 seconds, so one second leaves room for a slow
/// shutdown without ever outliving the grace period or hanging.
pub const GRACEFUL_STOP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// The status a signalled exit reports, matching `ProcessOutcome::Interrupted`.
///
/// A supervisor sees one exit code for "stopped before the turn finished",
/// whether the operator pressed Ctrl-C or the supervisor asked.
pub const SIGNALLED_EXIT_CODE: i32 = 4;

/// Whether a terminal record has already been written by the ordinary path.
///
/// A signal that races a natural exit must not append a second terminal record,
/// so whichever writes first wins and the other stays quiet.
static TERMINAL_WRITTEN: AtomicBool = AtomicBool::new(false);

/// Claims the right to write this stream's one terminal record.
///
/// The natural tail claims by calling [`terminal_record_written`] just before
/// it writes `bye`; the signal thread claims inside [`announce`]. Both go
/// through this one compare-and-exchange, so a stop that arrives while the
/// ordinary exit is running produces one terminator, not two.
fn claim_terminal_record_flag(flag: &AtomicBool) -> bool {
    flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Records that the ordinary path has written its terminal record.
///
/// After this, a signal exits without announcing, because the face has already
/// said what happened.
pub fn terminal_record_written() {
    let _ = claim_terminal_record_flag(&TERMINAL_WRITTEN);
}

/// Claims the right to write this stream's terminal record, and says whether it
/// was still free.
///
/// For a face that has to terminate a stream nobody else has: a failure that
/// lands after the invocation is admitted but before the drive reaches its own
/// tail leaves the reader at end of stream with nothing, which is
/// indistinguishable from the process being killed. `true` means write it.
#[must_use]
pub fn claim_terminal_record() -> bool {
    claim_terminal_record_flag(&TERMINAL_WRITTEN)
}

/// Which signal stopped this process, once one has.
///
/// The graceful path reads this so its own terminal record says a supervisor
/// stopped the run rather than ending as an ordinary quit, and so the exit
/// status matches the one a hard exit would have reported. `0` means no signal.
static STOPPED_BY: AtomicUsize = AtomicUsize::new(0);

/// How many terminating signals the watcher has seen.
///
/// The CLI face polls this from its observer, because the watcher is installed
/// before the face exists: a face that supplied its own counter could only be
/// armed once admission, catalog validation and the store open had all
/// finished, and a signal that arrived inside that window met the default
/// disposition and killed the process with an empty stream. See
/// [`signalled_stops`].
static SIGNALLED_STOPS: AtomicUsize = AtomicUsize::new(0);

/// How many terminating signals have arrived, for a face that polls.
///
/// A face reads this the way it reads its own Ctrl-C counter, and raises the
/// same interrupt. It only ever rises, and only from the watcher thread.
#[must_use]
pub fn signalled_stops() -> usize {
    SIGNALLED_STOPS.load(Ordering::Acquire)
}

const STOPPED_BY_TERMINATE: usize = 1;
const STOPPED_BY_HANGUP: usize = 2;

/// What the face should tell its reader, if a signal stopped this process.
///
/// `None` while the process is ending for any ordinary reason. A face reads
/// this once, on its way out, and passes the answer inward: the record is
/// process-wide, so a drive that reached for it directly could not be tested
/// alongside anything else in the same process.
#[must_use]
pub fn stop_reason() -> Option<&'static str> {
    match STOPPED_BY.load(Ordering::Acquire) {
        STOPPED_BY_TERMINATE => Some("plak was terminated by a signal before the turn completed"),
        STOPPED_BY_HANGUP => Some("plak lost its terminal before the turn completed"),
        _ => None,
    }
}

/// How many `SIGINT`s the JSONL face has received.
///
/// The CLI and TUI own Ctrl-C through their own paths. `plak agent` has no
/// terminal, so without a handler a `SIGINT` either killed it with no `bye` or,
/// inherited as ignored, did nothing while the turn ran on. It counts here and
/// the drive turns it into the same interrupt-then-quit an end of input sends.
static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

/// How many `SIGINT`s have arrived since [`count_interrupts`].
#[must_use]
pub fn interrupts() -> usize {
    INTERRUPTS.load(Ordering::Acquire)
}

/// Takes `SIGINT` for a face that polls [`interrupts`], replacing whatever
/// disposition the process inherited. Registration is live when this returns.
#[cfg(unix)]
pub fn count_interrupts() {
    use tokio::signal::unix::{SignalKind, signal};

    let (registered, confirmed) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("plak-interrupts".to_owned())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
                    return;
                };
                let _ = registered.try_send(());
                while interrupt.recv().await.is_some() {
                    INTERRUPTS.fetch_add(1, Ordering::Release);
                }
            });
        });
    if spawned.is_ok() {
        let _ = confirmed.recv();
    }
}

/// What one face says when a signal stops it.
///
/// Both messages are rendered before installation, because a signal handler may
/// not allocate or format.
#[derive(Clone, Copy, Debug)]
pub struct SignalAnnouncement {
    /// The descriptor the face's terminal record belongs on: 2 for a CLI
    /// diagnostic, 1 for a JSONL record stream.
    pub fd: i32,
    /// Written on `SIGTERM`.
    pub terminated: &'static [u8],
    /// Written on `SIGHUP`.
    pub hangup: &'static [u8],
    /// Whether to write before asking for the graceful stop.
    ///
    /// A CLI diagnostic says something the graceful path does not — that a
    /// supervisor stopped this run — so it goes out at once, and the interrupt
    /// line that follows is a second, different fact. A JSONL stream has room
    /// for exactly one terminal record, so its announcement waits: if the
    /// graceful stop emits its own `bye` in time, that is the one record, and
    /// this fires only when the deadline passes with the stream still open.
    pub announce_immediately: bool,
}

/// Starts the terminating-signal watcher for this face.
///
/// Failure to install leaves the default disposition, which is the behaviour the
/// face had before, and is never fatal.
#[cfg(unix)]
pub fn install<F>(announcement: SignalAnnouncement, request_stop: F)
where
    F: Fn() + Send + 'static,
{
    // Registration must be live before this returns. Handing the caller back
    // an unarmed process leaves a window in which the default disposition
    // still applies, and a supervisor that stops a slow start inside that
    // window kills it outright: no announcement, an empty stream and a stale
    // branch head. The window is short but it is real, and it is widest
    // exactly when a stop is most likely.
    let (registered, confirmed) = std::sync::mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("plak-stop-signals".to_owned())
        .spawn(move || watch(announcement, &request_stop, &registered))
        .is_ok()
    {
        // `Err` means the thread gave up before registering, which leaves the
        // default disposition: the behaviour the face had before, never fatal.
        let _ = confirmed.recv();
    }
}

#[cfg(unix)]
fn watch(
    announcement: SignalAnnouncement,
    request_stop: &dyn Fn(),
    registered: &std::sync::mpsc::SyncSender<()>,
) {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
    else {
        return;
    };
    let Some(text) = runtime.block_on(async {
        let (Ok(mut terminate), Ok(mut hangup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            return None;
        };
        // Both dispositions are ours from here, so the caller may proceed.
        let _ = registered.try_send(());
        let text = tokio::select! {
            _ = terminate.recv() => {
                STOPPED_BY.store(STOPPED_BY_TERMINATE, Ordering::Release);
                announcement.terminated
            }
            _ = hangup.recv() => {
                STOPPED_BY.store(STOPPED_BY_HANGUP, Ordering::Release);
                announcement.hangup
            }
        };
        SIGNALLED_STOPS.fetch_add(1, Ordering::Release);
        if announcement.announce_immediately {
            announce(announcement.fd, text);
        }
        Some(text)
    }) else {
        return;
    };
    // The record is out. Now ask for the same graceful stop Ctrl-C asks for, so
    // the turn is cancelled and joined and the branch head is left current: a
    // bare exit here leaves the session unresumable with a stale locator.
    request_stop();
    std::thread::sleep(GRACEFUL_STOP_DEADLINE);
    // Still here: the graceful stop did not finish in time and a supervisor's
    // patience is nearly up. A face that deferred its announcement has said
    // nothing yet, so it says it now, and then we leave rather than hang.
    if !announcement.announce_immediately {
        announce(announcement.fd, text);
    }
    std::process::exit(SIGNALLED_EXIT_CODE);
}

/// How long a signalled TUI may take to quit through its own loop before the
/// watcher restores the terminal itself and exits.
///
/// This backstop also covers a loop blocked before terminal restoration.
pub const INTERACTIVE_FORCED_EXIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// Starts the terminating-signal watcher for the interactive TUI.
///
/// The first `SIGTERM` or `SIGHUP` is only recorded: the event loop sees
/// [`signalled_stops`] rise and quits as `Ctrl+Q` would. A second signal, or
/// [`INTERACTIVE_FORCED_EXIT_DEADLINE`] passing after the first, calls
/// `force_restore` on the watcher thread and exits with
/// [`SIGNALLED_EXIT_CODE`]. Registration is live when this returns; a second
/// call is a no-op.
#[cfg(unix)]
pub fn install_interactive(force_restore: fn()) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let (registered, confirmed) = std::sync::mpsc::sync_channel(1);
        if std::thread::Builder::new()
            .name("plak-stop-signals".to_owned())
            .spawn(move || watch_interactive(force_restore, &registered))
            .is_ok()
        {
            let _ = confirmed.recv();
        }
    });
}

#[cfg(unix)]
fn watch_interactive(force_restore: fn(), registered: &std::sync::mpsc::SyncSender<()>) {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    else {
        return;
    };
    let stopped = runtime.block_on(async {
        let (Ok(mut terminate), Ok(mut hangup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            return false;
        };
        let _ = registered.try_send(());
        tokio::select! {
            _ = terminate.recv() => STOPPED_BY.store(STOPPED_BY_TERMINATE, Ordering::Release),
            _ = hangup.recv() => STOPPED_BY.store(STOPPED_BY_HANGUP, Ordering::Release),
        }
        SIGNALLED_STOPS.fetch_add(1, Ordering::Release);
        crate::tui_timing::quit_requested();
        let _ = tokio::time::timeout(INTERACTIVE_FORCED_EXIT_DEADLINE, async {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = hangup.recv() => {}
            }
        })
        .await;
        true
    });
    if stopped {
        force_restore();
        std::process::exit(SIGNALLED_EXIT_CODE);
    }
}

/// Writes one face's terminal record, unless the ordinary path wrote one first.
///
/// The write goes straight to the descriptor rather than through `Stdout` or
/// `Stderr`. Those carry a process-wide lock that the face itself holds while
/// it streams, and taking it here made the exit wait for the turn it was
/// supposed to cut short: measured at sixteen seconds against a supervisor's
/// two-second grace. A descriptor write cannot be blocked by our own printing.
#[cfg(unix)]
fn announce(fd: i32, text: &'static [u8]) {
    if !claim_terminal_record_flag(&TERMINAL_WRITTEN) {
        return;
    }
    let mut written = 0;
    while written < text.len() {
        let result = if fd == 1 {
            rustix::io::write(std::io::stdout(), &text[written..])
        } else {
            rustix::io::write(std::io::stderr(), &text[written..])
        };
        match result {
            Ok(0) | Err(_) => return,
            Ok(count) => written = written.saturating_add(count),
        }
    }
}

/// The CLI face's announcement: one diagnostic line on stderr.
#[must_use]
pub const fn cli_announcement() -> SignalAnnouncement {
    SignalAnnouncement {
        fd: 2,
        terminated: b"plak: [terminated]\n",
        hangup: b"plak: [hangup]\n",
        announce_immediately: true,
    }
}

/// The JSONL face's announcement: exactly one error record, then `bye`.
#[must_use]
pub const fn jsonl_announcement() -> SignalAnnouncement {
    SignalAnnouncement {
        fd: 1,
        terminated: b"{\"kind\":\"error\",\"message\":\"plak was terminated by a signal before the turn completed\",\"v\":1}\n{\"kind\":\"bye\",\"v\":1}\n",
        hangup: b"{\"kind\":\"error\",\"message\":\"plak lost its terminal before the turn completed\",\"v\":1}\n{\"kind\":\"bye\",\"v\":1}\n",
        announce_immediately: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both faces announce on the descriptor their records belong on.
    #[test]
    fn each_face_announces_where_its_records_go() {
        assert_eq!(cli_announcement().fd, 2);
        assert_eq!(jsonl_announcement().fd, 1);
    }

    /// The JSONL announcement is exactly one error record and one `bye`, each a
    /// complete line, so a reader that stops at `bye` sees a closed stream.
    #[test]
    fn the_jsonl_announcement_is_one_error_then_one_bye() {
        for bytes in [jsonl_announcement().terminated, jsonl_announcement().hangup] {
            let text = std::str::from_utf8(bytes).expect("utf-8");
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(lines.len(), 2, "{text}");
            let error: serde_json::Value =
                serde_json::from_str(lines[0]).expect("error record is JSON");
            assert_eq!(error["kind"], "error");
            assert_eq!(error["v"], 1);
            assert!(
                error["message"].as_str().is_some_and(|m| !m.is_empty()),
                "the record says what happened"
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(lines[1]).expect("bye is JSON"),
                serde_json::json!({"kind": "bye", "v": 1})
            );
            assert!(text.ends_with('\n'), "every record is a complete line");
        }
    }

    /// The CLI announcement is one complete diagnostic line per signal.
    #[test]
    fn the_cli_announcement_is_one_line_per_signal() {
        for bytes in [cli_announcement().terminated, cli_announcement().hangup] {
            let text = std::str::from_utf8(bytes).expect("utf-8");
            assert_eq!(text.lines().count(), 1, "{text}");
            assert!(text.starts_with("plak: "));
            assert!(text.ends_with('\n'));
        }
    }

    /// A signalled exit reports the same status as an interrupted one.
    #[test]
    fn a_signalled_exit_shares_the_interrupted_status() {
        assert_eq!(SIGNALLED_EXIT_CODE, 4);
    }

    /// Each face announces on the discipline its stream allows.
    ///
    /// The CLI speaks at once, because a diagnostic line costs nothing and the
    /// operator may never see anything else. JSONL waits, because its stream
    /// has room for exactly one terminal record and the graceful stop usually
    /// writes it first.
    #[test]
    fn the_cli_speaks_at_once_and_jsonl_waits_for_its_turn() {
        assert!(cli_announcement().announce_immediately);
        assert!(!jsonl_announcement().announce_immediately);
    }

    /// The deadline sits between the graceful stop and the supervisor's grace.
    ///
    /// Too short and a resumable session is cut off; too long and the process
    /// outlives the two seconds a supervisor allows before `SIGKILL`. The
    /// graceful path measures around 0.3 seconds on a live turn.
    #[test]
    fn the_deadline_leaves_room_for_the_graceful_stop_and_for_the_supervisor() {
        assert!(GRACEFUL_STOP_DEADLINE >= std::time::Duration::from_millis(600));
        assert!(GRACEFUL_STOP_DEADLINE <= std::time::Duration::from_millis(1_500));
    }

    /// A blocked TUI cannot exceed the same hard bound as voluntary quit.
    #[test]
    fn the_interactive_backstop_enforces_the_one_second_bound() {
        assert_eq!(
            INTERACTIVE_FORCED_EXIT_DEADLINE,
            std::time::Duration::from_secs(1)
        );
    }

    /// A signal racing a natural exit still yields exactly one `bye`, last.
    ///
    /// Both terminators are real: the drive's tail writes `bye` on stdout, and
    /// the signal thread writes an error record plus its own `bye` straight to
    /// the descriptor. Only the claim keeps them from both landing, in either
    /// arrival order.
    #[test]
    fn a_signal_racing_a_natural_exit_yields_exactly_one_bye_last() {
        let natural = b"{\"kind\":\"bye\",\"v\":1}\n";
        for signal_first in [false, true] {
            let flag = AtomicBool::new(false);
            let mut stream: Vec<u8> = Vec::new();
            let write_signal = |stream: &mut Vec<u8>| {
                if claim_terminal_record_flag(&flag) {
                    stream.extend_from_slice(jsonl_announcement().terminated);
                }
            };
            let write_natural = |stream: &mut Vec<u8>| {
                if claim_terminal_record_flag(&flag) {
                    stream.extend_from_slice(natural);
                }
            };
            if signal_first {
                write_signal(&mut stream);
                write_natural(&mut stream);
            } else {
                write_natural(&mut stream);
                write_signal(&mut stream);
            }
            let text = String::from_utf8(stream).expect("utf-8");
            let records: Vec<serde_json::Value> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).expect("each record is JSON"))
                .collect();
            let byes = records
                .iter()
                .filter(|record| record["kind"] == "bye")
                .count();
            assert_eq!(byes, 1, "signal_first={signal_first}: {records:?}");
            assert_eq!(
                records.last().expect("a terminator")["kind"],
                "bye",
                "signal_first={signal_first}: the terminator is last: {records:?}"
            );
        }
    }
}
