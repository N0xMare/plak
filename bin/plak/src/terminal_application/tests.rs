use super::*;
use std::cell::Cell;
#[cfg(all(unix, not(target_os = "macos")))]
use std::ffi::OsStr;
use std::io::Cursor;
use std::rc::Rc;
#[cfg(target_os = "linux")]
type TestResult = Result<(), Box<dyn Error>>;
const TEST_SECRET_LINE_LIMIT: usize = 16 * 1024;

struct FakeEchoGuard {
    restored: Rc<Cell<bool>>,
    fail_restore: bool,
}

impl TerminalEchoRestore for FakeEchoGuard {
    fn restore(&mut self) -> Result<(), ApplicationSecretInputError> {
        self.restored.set(true);
        if self.fail_restore {
            Err(ApplicationSecretInputError::EchoRestore)
        } else {
            Ok(())
        }
    }
}

#[test]
fn secret_line_is_single_line_bounded_utf8_and_payload_free() {
    let mut accepted =
        Cursor::new(b"http://localhost:1455/auth/callback?code=secret&state=state\r\nignored\n");
    let Ok(line) = read_secret_line(&mut accepted, TEST_SECRET_LINE_LIMIT) else {
        panic!("valid bounded redirect was rejected");
    };
    assert_eq!(
        line.as_str(),
        "http://localhost:1455/auth/callback?code=secret&state=state"
    );
    assert_eq!(format!("{line:?}"), "ApplicationSecretLine(<redacted>)");

    for (bytes, expected) in [
        (Vec::new(), ApplicationSecretInputError::Empty),
        (vec![0xff, b'\n'], ApplicationSecretInputError::InvalidUtf8),
        (
            vec![b'x'; TEST_SECRET_LINE_LIMIT + 1],
            ApplicationSecretInputError::TooLong,
        ),
        (vec![0x03], ApplicationSecretInputError::Cancelled),
    ] {
        assert_eq!(
            read_secret_line(&mut Cursor::new(bytes), TEST_SECRET_LINE_LIMIT).map(|_| ()),
            Err(expected)
        );
        assert!(!expected.to_string().contains("secret"));
    }
}

#[test]
fn secret_line_always_restores_echo_and_surfaces_restore_failure() {
    let restored = Rc::new(Cell::new(false));
    let mut output = Vec::new();
    let mut valid = Cursor::new(b"http://localhost/callback?code=x&state=y\n");
    let line = read_secret_line_with_guard(
        &mut valid,
        &mut output,
        TEST_SECRET_LINE_LIMIT,
        FakeEchoGuard {
            restored: Rc::clone(&restored),
            fail_restore: false,
        },
    );
    assert!(line.is_ok());
    assert!(restored.get());
    assert_eq!(output, b"\n");

    restored.set(false);
    let mut empty = Cursor::new(b"\n");
    assert_eq!(
        read_secret_line_with_guard(
            &mut empty,
            &mut output,
            TEST_SECRET_LINE_LIMIT,
            FakeEchoGuard {
                restored: Rc::clone(&restored),
                fail_restore: true,
            },
        )
        .map(|_| ()),
        Err(ApplicationSecretInputError::EchoRestore)
    );
    assert!(restored.get());
}

#[cfg(target_os = "linux")]
#[test]
fn unix_terminal_mode_hides_input_and_restores_exact_local_flags() -> TestResult {
    use rustix::pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt};

    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let master = openpt(flags)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let slave = ioctl_tiocgptpeer(&master, flags)?;
    let before = rustix::termios::tcgetattr(&slave)?;
    let original = disable_terminal_echo_on(&slave)?;
    let hidden = rustix::termios::tcgetattr(&slave)?;
    let removed = rustix::termios::LocalModes::ECHO
        | rustix::termios::LocalModes::ECHONL
        | rustix::termios::LocalModes::ISIG
        | rustix::termios::LocalModes::ICANON;
    assert!(!hidden.local_modes.intersects(removed));
    assert_eq!(original.local_modes, before.local_modes);
    restore_terminal_mode(&slave, &original)?;
    let restored = rustix::termios::tcgetattr(&slave)?;
    assert_eq!(restored.local_modes, before.local_modes);
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn local_browser_launch_uses_one_literal_argument_without_a_shell() {
    let url = "https://example.test/authorize?state=a&next=b";
    let command = browser_command(url);
    assert_eq!(command.get_program(), OsStr::new("xdg-open"));
    assert_eq!(command.get_args().collect::<Vec<_>>(), [OsStr::new(url)]);
}
