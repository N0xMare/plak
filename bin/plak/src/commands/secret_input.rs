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
mod tests;
