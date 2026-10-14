//! Bounded OAuth paste input with cooked terminal editing and signal handling.

use std::io;
use std::os::fd::AsFd;

use rustix::fs::OFlags;
use rustix::termios::{LocalModes, OptionalActions, Termios};
use zeroize::Zeroizing;

use crate::{CliFailure, CliResult};

pub(crate) struct SecretLine(Zeroizing<Vec<u8>>);

impl SecretLine {
    pub(crate) fn as_str(&self) -> &str {
        // Construction validates UTF-8 before this value can escape the reader.
        std::str::from_utf8(&self.0).unwrap_or_default().trim()
    }
}

impl std::fmt::Debug for SecretLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretLine(<redacted>)")
    }
}

enum ReadStep {
    Done,
    Progress,
    Pending,
}

struct LineBuffer {
    bytes: Zeroizing<Vec<u8>>,
    maximum: usize,
}

impl LineBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Zeroizing::new(Vec::with_capacity(maximum)),
            maximum,
        }
    }

    fn step(&mut self, input: &mut dyn io::BufRead) -> CliResult<ReadStep> {
        // Count raw bytes, including whitespace and the line terminator. Never
        // read or allocate beyond the bound, even for an endless input stream.
        if self.bytes.len() == self.maximum {
            return Err(CliFailure::new(
                "remote callback URL is too long; start a fresh login",
            ));
        }
        let mut byte = Zeroizing::new([0_u8; 1]);
        match input.read(&mut *byte) {
            Ok(0) if self.bytes.is_empty() => Err(CliFailure::new(
                "remote callback input was cancelled; start a fresh login",
            )),
            Ok(0) => Ok(ReadStep::Done),
            Ok(_) => {
                self.bytes.push(byte[0]);
                if matches!(byte[0], 3 | 4) {
                    return Err(CliFailure::new("login cancelled"));
                }
                Ok(if byte[0] == b'\n' {
                    ReadStep::Done
                } else {
                    ReadStep::Progress
                })
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(ReadStep::Pending)
            }
            Err(_) => Err(CliFailure::new("could not read remote callback input")),
        }
    }

    fn finish(self) -> CliResult<SecretLine> {
        let text = std::str::from_utf8(&self.bytes)
            .map_err(|_| CliFailure::new("remote callback input is not valid UTF-8"))?;
        if text.trim().is_empty() {
            return Err(CliFailure::new(
                "remote callback URL is required; paste the full redirect URL then Enter",
            ));
        }
        Ok(SecretLine(self.bytes))
    }
}

fn prompt(stderr: &mut dyn io::Write) -> CliResult<()> {
    write!(stderr, "> ")
        .and_then(|()| stderr.flush())
        .map_err(|_| CliFailure::new("stderr failed"))
}

pub(super) fn read_plain(
    input: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
    maximum: usize,
) -> CliResult<SecretLine> {
    prompt(stderr)?;
    let mut line = LineBuffer::new(maximum);
    loop {
        match line.step(input)? {
            ReadStep::Done => break,
            ReadStep::Progress => {}
            ReadStep::Pending => {
                return Err(CliFailure::new("could not read remote callback input"));
            }
        }
    }
    line.finish()
}

pub(super) fn read_system(
    input: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
    maximum: usize,
) -> CliResult<SecretLine> {
    let stdin = io::stdin();
    if !rustix::termios::isatty(&stdin) {
        return read_plain(input, stderr, maximum);
    }
    read_terminal(&stdin, input, stderr, maximum)
}

fn read_terminal(
    terminal: &impl AsFd,
    input: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
    maximum: usize,
) -> CliResult<SecretLine> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| CliFailure::new("could not initialize login cancellation"))?;
    runtime.block_on(async {
        // Install the handler before hiding echo. ISIG stays enabled, so Ctrl-C
        // reaches this signal stream instead of becoming a secret input byte.
        let mut interrupts =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .map_err(|_| CliFailure::new("could not initialize login cancellation"))?;
        let mut guard = CookedEchoGuard::new(terminal)?;
        let result = async {
            prompt(stderr)?;
            let mut line = LineBuffer::new(maximum);
            loop {
                tokio::select! {
                    biased;
                    _ = interrupts.recv() => return Err(CliFailure::new("login cancelled")),
                    () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                        // Buffered bytes are consumed before consulting the fd;
                        // this also preserves input prefetched at admission.
                        loop {
                            match line.step(input)? {
                                ReadStep::Done => return line.finish(),
                                ReadStep::Progress => {},
                                ReadStep::Pending => break,
                            }
                        }
                    }
                }
            }
        }
        .await;
        guard.discard_input = result.is_err();
        guard.restore()?;
        writeln!(stderr).map_err(|_| CliFailure::new("stderr failed"))?;
        result
    })
}

struct CookedEchoGuard<'a, F: AsFd> {
    terminal: &'a F,
    original: Termios,
    flags: OFlags,
    restored: bool,
    discard_input: bool,
}

impl<'a, F: AsFd> CookedEchoGuard<'a, F> {
    fn new(terminal: &'a F) -> CliResult<Self> {
        let original = rustix::termios::tcgetattr(terminal)
            .map_err(|_| CliFailure::new("could not read terminal mode"))?;
        let flags = rustix::fs::fcntl_getfl(terminal)
            .map_err(|_| CliFailure::new("could not read terminal flags"))?;
        let guard = Self {
            terminal,
            original,
            flags,
            restored: false,
            discard_input: true,
        };
        let mut hidden = guard.original.clone();
        hidden
            .local_modes
            .remove(LocalModes::ECHO | LocalModes::ECHONL);
        rustix::termios::tcsetattr(terminal, OptionalActions::Now, &hidden)
            .map_err(|_| CliFailure::new("could not disable terminal echo"))?;
        rustix::fs::fcntl_setfl(terminal, flags | OFlags::NONBLOCK)
            .map_err(|_| CliFailure::new("could not enable cancellable terminal input"))?;
        Ok(guard)
    }

    fn restore(&mut self) -> CliResult<()> {
        if self.restored {
            return Ok(());
        }
        // Refusal, cancellation and unwinding must not return queued secrets
        // to the next cooked reader after echo is restored.
        let flush = if self.discard_input {
            rustix::termios::tcflush(self.terminal, rustix::termios::QueueSelector::IFlush)
        } else {
            Ok(())
        };
        let mode = rustix::termios::tcsetattr(self.terminal, OptionalActions::Now, &self.original);
        let flags = rustix::fs::fcntl_setfl(self.terminal, self.flags);
        flush
            .and(mode)
            .and(flags)
            .map_err(|_| CliFailure::new("could not restore terminal mode"))?;
        self.restored = true;
        Ok(())
    }
}

impl<F: AsFd> Drop for CookedEchoGuard<'_, F> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::io::{BufReader, Read, Write};

    #[test]
    fn raw_bound_stops_before_reading_or_trimming_unbounded_input() {
        struct Endless {
            read: usize,
            byte: u8,
        }
        impl Read for Endless {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert!(self.read < 16, "reader crossed the raw input bound");
                assert_eq!(buffer.len(), 1);
                buffer[0] = self.byte;
                self.read += 1;
                Ok(1)
            }
        }
        impl io::BufRead for Endless {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                panic!("must use bounded reads")
            }
            fn consume(&mut self, _: usize) {
                panic!("must use bounded reads")
            }
        }
        for byte in *b"x \t" {
            let mut input = Endless { read: 0, byte };
            let error = read_plain(&mut input, &mut Vec::new(), 16).expect_err("raw bound");
            assert!(error.message().contains("too long"));
            assert_eq!(input.read, 16);
        }
        let mut input = b" padded \n".as_slice();
        let secret = read_plain(&mut input, &mut Vec::new(), 9).expect("bounded line");
        assert_eq!(secret.as_str(), "padded");
        assert_eq!(format!("{secret:?}"), "SecretLine(<redacted>)");
    }

    // A fresh process owns both the signal handler and its controlling PTY.
    // The ordinary offline test never touches the developer's terminal.
    #[test]
    fn pty_secret_child() -> Result<(), Box<dyn std::error::Error>> {
        struct PromptProbe;
        impl Write for PromptProbe {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                io::stderr().write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                let hidden = rustix::termios::tcgetattr(io::stdin())?;
                assert!(
                    !hidden
                        .local_modes
                        .intersects(LocalModes::ECHO | LocalModes::ECHONL)
                );
                assert!(
                    hidden
                        .local_modes
                        .contains(LocalModes::ISIG | LocalModes::ICANON)
                );
                io::stderr().flush()
            }
        }
        let Ok(scenario) = env::var("PLAK_TEST_OAUTH_PTY") else {
            return Ok(());
        };
        let stdin = io::stdin();
        let before = rustix::termios::tcgetattr(&stdin)?;
        let flags = rustix::fs::fcntl_getfl(&stdin)?;
        if scenario == "panic" {
            struct PanicInput;
            impl Read for PanicInput {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    panic!("synthetic reader failure")
                }
            }
            impl io::BufRead for PanicInput {
                fn fill_buf(&mut self) -> io::Result<&[u8]> {
                    panic!("synthetic reader failure")
                }
                fn consume(&mut self, _: usize) {}
            }
            assert!(
                std::panic::catch_unwind(|| {
                    let _ = read_system(&mut PanicInput, &mut PromptProbe, 16);
                })
                .is_err()
            );
        } else {
            let result = read_system(&mut stdin.lock(), &mut PromptProbe, 16);
            match scenario.as_str() {
                "success" => assert_eq!(result.expect("synthetic paste").as_str().len(), 12),
                "bound" => assert!(result.expect_err("bound").message().contains("too long")),
                "cancel" => assert!(result.expect_err("cancel").message().contains("cancelled")),
                _ => panic!("unknown PTY scenario"),
            }
        }
        let restored = rustix::termios::tcgetattr(&stdin)?;
        assert_eq!(restored.local_modes, before.local_modes);
        assert_eq!(restored.input_modes, before.input_modes);
        assert_eq!(restored.output_modes, before.output_modes);
        assert_eq!(restored.control_modes, before.control_modes);
        assert_eq!(format!("{restored:?}"), format!("{before:?}"));
        assert_eq!(rustix::fs::fcntl_getfl(&stdin)?, flags);
        writeln!(io::stderr(), "RESTORED")?;
        Ok(())
    }

    #[test]
    fn owned_pty_hides_paste_and_restores_after_success_refusal_cancel_and_panic()
    -> Result<(), Box<dyn std::error::Error>> {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        use std::sync::mpsc;
        for scenario in ["success", "bound", "cancel", "panic"] {
            let pair = native_pty_system().openpty(PtySize::default())?;
            let mut command = CommandBuilder::new(env::current_exe()?);
            command.args([
                "--exact",
                "commands::secret_input::tests::pty_secret_child",
                "--nocapture",
            ]);
            command.env("PLAK_TEST_OAUTH_PTY", scenario);
            let mut child = pair.slave.spawn_command(command)?;
            let mut writer = pair.master.take_writer()?;
            let reader = pair.master.try_clone_reader()?;
            let (tx, rx) = mpsc::channel();
            let join = std::thread::spawn(move || {
                let mut reader = BufReader::new(reader);
                loop {
                    let mut bytes = [0; 256];
                    match reader.read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(bytes[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            let mut output = Vec::new();
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                while !output.windows(2).any(|bytes| bytes == b"> ") {
                    output.extend(rx.recv_timeout(std::time::Duration::from_secs(10))?);
                }
                if scenario != "panic" {
                    match scenario {
                        "success" => writer.write_all(b"synthetic123\n")?,
                        "bound" => writer.write_all(b"                                \n")?,
                        "cancel" => writer.write_all(b"synthetic123\x03")?,
                        _ => unreachable!(),
                    }
                    writer.flush()?;
                }
                while !output.windows(8).any(|bytes| bytes == b"RESTORED") {
                    output.extend(rx.recv_timeout(std::time::Duration::from_secs(10))?);
                }
                assert!(!output.windows(12).any(|bytes| bytes == b"synthetic123"));
                Ok(())
            })();
            if result.is_err() {
                let _ = child.kill();
            }
            let status = child.wait()?;
            drop(writer);
            drop(pair);
            join.join().map_err(|_| "PTY reader panicked")?;
            result?;
            assert!(status.success());
        }
        Ok(())
    }
}
