//! Plak terminal input and browser launch adapters.

use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, IsTerminal as _};
use zeroize::Zeroizing;

/// One bounded secret line read at the application edge.
///
/// The owned text is zeroized on drop and its diagnostics never expose the
/// captured value.
pub struct ApplicationSecretLine {
    value: Zeroizing<String>,
}

impl ApplicationSecretLine {
    /// Borrows the captured line for immediate one-use completion.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.value.as_str()
    }
}

impl fmt::Debug for ApplicationSecretLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApplicationSecretLine(<redacted>)")
    }
}

/// Stable, payload-free failure while reading one application secret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ApplicationSecretInputError {
    /// The input stream failed.
    Input,
    /// A terminal cancellation byte was observed.
    Cancelled,
    /// The line exceeded its caller-provided byte limit.
    TooLong,
    /// EOF or a newline arrived before any bytes.
    Empty,
    /// The line was not valid UTF-8.
    InvalidUtf8,
    /// Terminal echo could not be disabled safely.
    EchoDisable,
    /// The exact original terminal mode could not be restored.
    EchoRestore,
    /// The post-input presentation newline could not be written.
    Output,
}

impl ApplicationSecretInputError {
    /// Returns a fixed operator-facing diagnostic containing no captured data.
    #[must_use]
    pub const fn operator_message(self) -> &'static str {
        match self {
            Self::Input => "remote callback input failed",
            Self::Cancelled => "remote callback input was cancelled; start a fresh login",
            Self::TooLong => "remote callback URL is too long; start a fresh login",
            Self::Empty => "remote callback URL is required; start a fresh login",
            Self::InvalidUtf8 => "remote callback URL is not valid UTF-8; start a fresh login",
            Self::EchoDisable => "terminal echo could not be disabled",
            Self::EchoRestore => "terminal echo could not be restored",
            Self::Output => "stderr failed",
        }
    }
}

impl fmt::Display for ApplicationSecretInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.operator_message())
    }
}

impl Error for ApplicationSecretInputError {}

/// Reads one bounded line while suppressing terminal echo when stdin is a TTY.
///
/// Bytes copied by this function are owned by zeroizing buffers. The caller's
/// operating-system or `BufRead` buffer is outside this function's erasure
/// authority. Terminal restoration is attempted after every read outcome, and
/// a restoration failure takes precedence over the input result. Application
/// callers must pass the process stdin reader when stdin is a terminal;
/// alternate readers are supported for non-terminal input and deterministic
/// tests.
///
/// # Errors
///
/// Returns a fixed payload-free error for terminal, input, bound, UTF-8, or
/// presentation failures.
pub fn read_terminal_secret_line(
    input: &mut dyn BufRead,
    output: &mut dyn io::Write,
    maximum_bytes: usize,
) -> Result<ApplicationSecretLine, ApplicationSecretInputError> {
    let echo_guard = disable_terminal_echo()?;
    read_secret_line_with_guard(input, output, maximum_bytes, echo_guard)
}

fn read_secret_line(
    input: &mut dyn BufRead,
    maximum_bytes: usize,
) -> Result<ApplicationSecretLine, ApplicationSecretInputError> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(maximum_bytes.min(1024)));
    loop {
        let available = input
            .fill_buf()
            .map_err(|_| ApplicationSecretInputError::Input)?;
        if available.is_empty() {
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let data_len = newline.unwrap_or(available.len());
        if available[..data_len]
            .iter()
            .any(|byte| matches!(*byte, 0x03 | 0x04 | 0x1a | 0x1c))
        {
            input.consume(data_len + usize::from(newline.is_some()));
            return Err(ApplicationSecretInputError::Cancelled);
        }
        if bytes.len().saturating_add(data_len) > maximum_bytes {
            return Err(ApplicationSecretInputError::TooLong);
        }
        bytes.extend_from_slice(&available[..data_len]);
        input.consume(data_len + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    if bytes.is_empty() {
        return Err(ApplicationSecretInputError::Empty);
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| ApplicationSecretInputError::InvalidUtf8)?
        .to_owned();
    Ok(ApplicationSecretLine {
        value: Zeroizing::new(value),
    })
}

trait TerminalEchoRestore {
    fn restore(&mut self) -> Result<(), ApplicationSecretInputError>;
}

fn read_secret_line_with_guard(
    input: &mut dyn BufRead,
    output: &mut dyn io::Write,
    maximum_bytes: usize,
    mut echo_guard: impl TerminalEchoRestore,
) -> Result<ApplicationSecretLine, ApplicationSecretInputError> {
    let line = read_secret_line(input, maximum_bytes);
    let restored = echo_guard.restore();
    let newline = writeln!(output).map_err(|_| ApplicationSecretInputError::Output);
    restored?;
    newline?;
    line
}

#[cfg(unix)]
struct PlatformTerminalEchoGuard {
    original: Option<rustix::termios::Termios>,
}

#[cfg(unix)]
impl TerminalEchoRestore for PlatformTerminalEchoGuard {
    fn restore(&mut self) -> Result<(), ApplicationSecretInputError> {
        let Some(original) = self.original.as_ref() else {
            return Ok(());
        };
        restore_terminal_mode(io::stdin(), original)
            .map_err(|_| ApplicationSecretInputError::EchoRestore)?;
        self.original = None;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for PlatformTerminalEchoGuard {
    fn drop(&mut self) {
        let _restored = self.restore();
    }
}

#[cfg(unix)]
fn disable_terminal_echo() -> Result<PlatformTerminalEchoGuard, ApplicationSecretInputError> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Ok(PlatformTerminalEchoGuard { original: None });
    }
    let original =
        disable_terminal_echo_on(&stdin).map_err(|_| ApplicationSecretInputError::EchoDisable)?;
    Ok(PlatformTerminalEchoGuard {
        original: Some(original),
    })
}

#[cfg(unix)]
fn disable_terminal_echo_on(
    fd: impl rustix::fd::AsFd,
) -> rustix::io::Result<rustix::termios::Termios> {
    let fd = fd.as_fd();
    let original = rustix::termios::tcgetattr(fd)?;
    let mut hidden = original.clone();
    hidden.local_modes.remove(
        rustix::termios::LocalModes::ECHO
            | rustix::termios::LocalModes::ECHONL
            | rustix::termios::LocalModes::ISIG
            | rustix::termios::LocalModes::ICANON,
    );
    rustix::termios::tcsetattr(fd, rustix::termios::OptionalActions::Now, &hidden)?;
    Ok(original)
}

#[cfg(unix)]
fn restore_terminal_mode(
    fd: impl rustix::fd::AsFd,
    original: &rustix::termios::Termios,
) -> rustix::io::Result<()> {
    rustix::termios::tcsetattr(fd, rustix::termios::OptionalActions::Now, original)
}

#[cfg(not(unix))]
struct PlatformTerminalEchoGuard;

#[cfg(not(unix))]
impl TerminalEchoRestore for PlatformTerminalEchoGuard {
    fn restore(&mut self) -> Result<(), ApplicationSecretInputError> {
        Ok(())
    }
}

#[cfg(not(unix))]
fn disable_terminal_echo() -> Result<PlatformTerminalEchoGuard, ApplicationSecretInputError> {
    if io::stdin().is_terminal() {
        Err(ApplicationSecretInputError::EchoDisable)
    } else {
        Ok(PlatformTerminalEchoGuard)
    }
}

/// Requests opening one provider authorization URL with the host's browser.
///
/// The URL is passed as one literal argument without a shell. A `true` result
/// means the opener process was spawned, not that authorization completed.
#[must_use]
#[cfg(any(unix, windows))]
pub fn request_browser_open(url: &str) -> bool {
    let mut command = browser_command(url);
    let Ok(mut child) = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let _reaper = std::thread::spawn(move || {
        let _status = child.wait();
    });
    true
}

/// Reports that no supported browser opener exists on this host target.
#[must_use]
#[cfg(not(any(unix, windows)))]
pub fn request_browser_open(_url: &str) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn browser_command(url: &str) -> std::process::Command {
    let mut command = std::process::Command::new("open");
    command.arg(url);
    command
}

#[cfg(all(unix, not(target_os = "macos")))]
fn browser_command(url: &str) -> std::process::Command {
    let mut command = std::process::Command::new("xdg-open");
    command.arg(url);
    command
}

#[cfg(windows)]
fn browser_command(url: &str) -> std::process::Command {
    let mut command = std::process::Command::new("rundll32");
    command.arg("url.dll,FileProtocolHandler").arg(url);
    command
}

#[cfg(test)]
mod tests {
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
        let mut accepted = Cursor::new(
            b"http://localhost:1455/auth/callback?code=secret&state=state\r\nignored\n",
        );
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
}
