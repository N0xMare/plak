//! Real PTY checks for ownership release, panic restoration and partial init.

use super::super::terminal_panic;
use super::*;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, channel};

const OWNER_PAYLOAD: &str = "OWNER_PANIC_PAYLOAD";
const CLOSED_PAYLOAD: &str = "PREVIOUS_HOOK_AFTER_CLOSE";
const PRIVATE_PAYLOAD: &str = "PRIVATE_WORKER_PANIC_PAYLOAD";

fn cooked_echo() {
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    writeln!(stdout(), "COOKED_READY").unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    assert_eq!(line.trim_end(), "echo-after-restoration");
    writeln!(stdout(), "COOKED_ECHO_OK").unwrap();
}

fn closed_child(force: bool) {
    let terminal = PersistentTerminal::open(true).unwrap();
    if force {
        force_restore_terminal();
        assert_eq!(terminal_panic::owner(), None);
        assert!(
            std::thread::spawn(|| std::panic::panic_any(CLOSED_PAYLOAD))
                .join()
                .is_err()
        );
        assert_eq!(terminal.close(), [] as [String; 0]);
    } else {
        assert_eq!(terminal.close(), [] as [String; 0]);
        assert_eq!(terminal_panic::owner(), None);
        assert!(
            std::thread::spawn(|| std::panic::panic_any(CLOSED_PAYLOAD))
                .join()
                .is_err()
        );
    }
    cooked_echo();
}

fn owner_panic_child() {
    let mut terminal = PersistentTerminal::open(true).unwrap();
    let mut app = App::new(super::super::test_support::launch_basic(), false);
    app.after_draw = Some(|| std::panic::panic_any(OWNER_PAYLOAD));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_app(&mut terminal.terminal, app, None, None, true)
    }));
    assert!(result.is_err());
    // Keep the guard alive: its Drop must not mask a missing hook restore.
    cooked_echo();
    assert_eq!(terminal.close(), [] as [String; 0]);
}

fn init_failure_child(target: InitStep) {
    install_terminal_panic_hook();
    let mut seen = Vec::new();
    let result = initialize_terminal(|step| {
        seen.push(step);
        if step == target {
            Err(TuiError::Io("injected init failure".into()))
        } else {
            Ok(())
        }
    });
    assert!(result.is_err());
    assert_eq!(seen.last(), Some(&target));
    assert_eq!(terminal_panic::owner(), None);
    writeln!(stdout(), "INIT_ERROR_HANDLED").unwrap();
    cooked_echo();
}

fn worker_failure_child() -> ! {
    let terminal = PersistentTerminal::open(false).unwrap();
    assert!(
        std::thread::Builder::new()
            .name("older-worker".into())
            .spawn(|| std::panic::panic_any(PRIVATE_PAYLOAD))
            .unwrap()
            .join()
            .is_err()
    );
    let worker = std::thread::Builder::new()
        .name("actionable-worker".into())
        .spawn(|| -> crate::CliResult<()> { std::panic::panic_any(PRIVATE_PAYLOAD) })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !worker.is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(terminal_panic::take_background_panic_report().is_none());
    assert_eq!(terminal.close(), [] as [String; 0]);
    let (actions, _input) = crate::surface_protocol::user_action_channel();
    crate::commands::support::exit_restored_tui(
        worker,
        actions,
        crate::tui::report_background_panic,
    );
}

#[test]
fn lifecycle_pty_child() {
    let Ok(scenario) = std::env::var("PLAK_TERMINAL_LIFECYCLE_TEST") else {
        return;
    };
    match scenario.as_str() {
        "close" => closed_child(false),
        "force" => closed_child(true),
        "owner-panic" => owner_panic_child(),
        "init-raw" => init_failure_child(InitStep::RawMode),
        "init-screen" => init_failure_child(InitStep::AlternateScreen),
        "worker-failure" => worker_failure_child(),
        _ => panic!("unknown lifecycle scenario"),
    }
}

struct PtyChild {
    _master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    received: Receiver<Vec<u8>>,
    output: Vec<u8>,
}

impl PtyChild {
    fn spawn(scenario: &str) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 35,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "tui::ui::terminal::lifecycle_tests::lifecycle_pty_child",
            "--nocapture",
        ]);
        command.env("PLAK_TERMINAL_LIFECYCLE_TEST", scenario);
        command.env("TERM", "xterm-256color");
        command.env("RUST_BACKTRACE", "0");
        command.env("PLAK_LOG", "");
        command.env("OTEL_EXPORTER_OTLP_ENDPOINT", "");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (chunks, received) = channel();
        std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 || chunks.send(bytes[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            _master: pair.master,
            writer,
            child,
            received,
            output: Vec::new(),
        }
    }

    fn wait_for(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !String::from_utf8_lossy(&self.output).contains(marker) {
            let bytes = self
                .received
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "waiting for {marker}: {error}: {}",
                        String::from_utf8_lossy(&self.output)
                    )
                });
            self.output.extend(bytes);
        }
    }

    fn echo(&mut self) {
        self.wait_for("COOKED_READY");
        self.writer.write_all(b"echo-after-restoration\n").unwrap();
        self.wait_for("COOKED_ECHO_OK");
        assert!(
            String::from_utf8_lossy(&self.output).contains("echo-after-restoration"),
            "cooked terminal did not echo input"
        );
    }

    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                loop {
                    match self
                        .received
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(bytes) => self.output.extend(bytes),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(error) => panic!("PTY output did not close: {error}"),
                    }
                }
                assert!(
                    status.success(),
                    "{}",
                    String::from_utf8_lossy(&self.output)
                );
                return;
            }
            assert!(Instant::now() < deadline, "PTY child did not exit");
            if let Ok(bytes) = self.received.recv_timeout(Duration::from_millis(20)) {
                self.output.extend(bytes);
            }
        }
    }

    fn position(&self, needle: &[u8]) -> usize {
        self.output
            .windows(needle.len())
            .position(|bytes| bytes == needle)
            .unwrap_or_else(|| {
                panic!(
                    "missing {needle:?}: {}",
                    String::from_utf8_lossy(&self.output)
                )
            })
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn normal_close_releases_owner_and_returns_panics_to_previous_hook() {
    let mut child = PtyChild::spawn("close");
    child.wait_for(CLOSED_PAYLOAD);
    assert!(child.position(b"\x1b[?1049l") < child.position(CLOSED_PAYLOAD.as_bytes()));
    child.echo();
    child.finish();
}

#[test]
fn forced_restore_releases_owner_and_returns_panics_to_previous_hook() {
    let mut child = PtyChild::spawn("force");
    child.wait_for(CLOSED_PAYLOAD);
    assert!(child.position(b"\x1b[?1049l") < child.position(CLOSED_PAYLOAD.as_bytes()));
    child.echo();
    child.finish();
}

#[test]
fn active_owner_panic_restores_modes_before_payload_and_leaves_cooked_echo() {
    let mut child = PtyChild::spawn("owner-panic");
    child.wait_for(OWNER_PAYLOAD);
    let payload = child.position(OWNER_PAYLOAD.as_bytes());
    for mode in [
        b"\x1b[?1000l".as_slice(),
        b"\x1b[?2004l",
        b"\x1b[<1u",
        b"\x1b[?1004l",
        b"\x1b[?1049l",
    ] {
        assert!(
            child.position(mode) < payload,
            "mode was restored after panic output"
        );
    }
    child.echo();
    child.finish();
}

#[test]
fn partial_initialization_failures_restore_after_raw_and_alternate_steps() {
    for scenario in ["init-raw", "init-screen"] {
        let mut child = PtyChild::spawn(scenario);
        child.wait_for("INIT_ERROR_HANDLED");
        assert!(child.position(b"\x1b[?1049l") < child.position(b"INIT_ERROR_HANDLED"));
        child.echo();
        child.finish();
    }
}

#[test]
fn failed_worker_reports_latest_source_site_after_restore_without_diagnostics() {
    let mut child = PtyChild::spawn("worker-failure");
    child.wait_for("most recent background panic:");
    child.finish();
    let output = String::from_utf8_lossy(&child.output);
    assert!(
        output.contains("actionable-worker") && output.contains("terminal/lifecycle_tests.rs"),
        "{output}"
    );
    assert!(
        !output.contains(PRIVATE_PAYLOAD) && !output.contains("older-worker"),
        "{output}"
    );
    assert_eq!(output.matches("most recent background panic:").count(), 1);
    assert!(child.position(b"\x1b[?1049l") < child.position(b"most recent background panic:"));
}
