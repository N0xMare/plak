//! A real terminal loop must paint a reader completion without another input event.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

const COMPLETED: &str = "background decoration completed";

#[test]
fn usage_decoration_wake_child() {
    let Some(root) = std::env::var_os("PLAK_USAGE_WAKE_TEST") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let mut launch = super::test_support::launch_basic();
    launch.usage_decoration = Some(crate::config::UsageDecorationReader::with_reader(
        move |_| {
            std::fs::write(root.join("entered"), "ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !root.join("release").exists() {
                assert!(Instant::now() < deadline, "reader was never released");
                std::thread::sleep(Duration::from_millis(5));
            }
            vec![COMPLETED.into()]
        },
        crate::tui::wake::wake,
    ));
    let mut app = super::App::new(launch, false);
    app.control_tab = Some(super::ControlTab::Usage);
    app.apply_host_event(super::HostEvent::UsageLimits(
        crate::tui::providers::unread_usage_limits(),
    ));
    let mut terminal = super::terminal::PersistentTerminal::open(false).unwrap();
    super::terminal::run_app(&mut terminal.terminal, app, None, None, false).unwrap();
    assert_eq!(terminal.close(), [] as [String; 0]);
}

struct Child(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn run_app_paints_usage_decoration_through_the_wake_pipe() {
    let root = tempfile::tempdir().unwrap();
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
        "tui::ui::usage_wake_tests::usage_decoration_wake_child",
        "--nocapture",
    ]);
    command.env("PLAK_USAGE_WAKE_TEST", root.path());
    command.env("TERM", "xterm-256color");
    let _child = Child(pair.slave.spawn_command(command).unwrap());
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (chunks, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 || chunks.send(bytes[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut screen = vt100::Parser::new(35, 120, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.path().join("entered").exists() || !screen.screen().contents().contains("Usage") {
        assert!(
            Instant::now() < deadline,
            "reader/terminal did not start: {}",
            screen.screen().contents()
        );
        if let Ok(bytes) = received.recv_timeout(Duration::from_millis(20)) {
            screen.process(&bytes);
        }
    }
    // No terminal or host event is sent: only the background reader can wake this loop.
    std::fs::write(root.path().join("release"), "go").unwrap();
    while !screen.screen().contents().contains(COMPLETED) {
        assert!(
            Instant::now() < deadline,
            "completion was not painted: {}",
            screen.screen().contents()
        );
        if let Ok(bytes) = received.recv_timeout(Duration::from_millis(20)) {
            screen.process(&bytes);
        }
    }
    pair.master
        .take_writer()
        .unwrap()
        .write_all(&[0x11])
        .unwrap();
}
