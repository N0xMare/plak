//! The public bridge checks exact-version Hello before discovery, never runtime custody.

use std::io::{self, BufRead as _, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::surface_protocol::PlakExtension;
use crate::wire_transport::hosts;
use crate::{CliFailure, CliResult, ProcessOutcome, product_paths::PlakPaths};
use plakous_host_protocol::wire::{
    BridgeUnavailableReason, ClientFrame, HandshakeRefusal, HostFrame, MAX_LINE_BYTES,
    WIRE_VERSION, decode_line, encode_line,
};
use plakous_hosted::host_service::wire::HELLO_DEADLINE;

const DISCOVERY_WINDOW: Duration = Duration::from_secs(10);

pub(super) fn connect(paths: &PlakPaths, auto_start: bool) -> CliResult<ProcessOutcome> {
    let line = match hello() {
        Ok(line) => line,
        Err(reason) => {
            refuse(reason)?;
            return Ok(ProcessOutcome::Success);
        }
    };
    let Ok(ClientFrame::<PlakExtension>::Hello(hello)) = decode_line(&line) else {
        refuse(HandshakeRefusal::Malformed {
            detail: "the first frame must be hello".into(),
        })?;
        return Ok(ProcessOutcome::Success);
    };
    if hello.proto != WIRE_VERSION {
        refuse(HandshakeRefusal::VersionMismatch {
            host: WIRE_VERSION,
            client: hello.proto,
        })?;
        return Ok(ProcessOutcome::Success);
    }
    let stream = match discover(paths, auto_start) {
        Ok(stream) => stream,
        Err(error) => {
            let (reason, failure) = *error;
            let retry_after_ms = match reason {
                BridgeUnavailableReason::Absent
                | BridgeUnavailableReason::Starting
                | BridgeUnavailableReason::Stopping => Some(500),
                BridgeUnavailableReason::IncompatibleState
                | BridgeUnavailableReason::VerificationFailed
                | BridgeUnavailableReason::StartRefused => None,
            };
            refuse(HandshakeRefusal::BridgeUnavailable {
                reason,
                retry_after_ms,
            })?;
            return Err(failure);
        }
    };
    relay(stream, &line)
}

fn hello() -> Result<String, HandshakeRefusal> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = io::stdin()
            .lock()
            .take(MAX_LINE_BYTES as u64)
            .read_until(b'\n', &mut bytes)
            .ok()
            .filter(|_| bytes.last() == Some(&b'\n'))
            .and_then(|_| String::from_utf8(bytes).ok());
        let _ = send.send(result);
    });
    receive
        .recv_timeout(HELLO_DEADLINE)
        .ok()
        .flatten()
        .ok_or_else(|| HandshakeRefusal::Malformed {
            detail: "a bounded newline-terminated hello is required within 10000 ms".into(),
        })
}

fn refuse(reason: HandshakeRefusal) -> CliResult<()> {
    let line = encode_line(&HostFrame::<PlakExtension>::Refused(reason))
        .map_err(|_| CliFailure::new("cannot encode bridge refusal"))?;
    let mut output = io::stdout().lock();
    output
        .write_all(line.as_bytes())
        .and_then(|()| output.flush())
        .map_err(failed)
}

type DiscoveryResult = Result<UnixStream, Box<(BridgeUnavailableReason, CliFailure)>>;

fn discover(paths: &PlakPaths, auto_start: bool) -> DiscoveryResult {
    let verify = |failure| Box::new((BridgeUnavailableReason::VerificationFailed, failure));
    let deadline = Instant::now()
        + if auto_start {
            Duration::from_secs(30)
        } else {
            DISCOVERY_WINDOW
        };
    let key =
        crate::carry::upgrade::receipt::successor_key(paths, &crate::home::state::state_key())
            .map_err(verify)?;
    let socket = paths.host_run_dir().join(format!("{key}.sock"));
    let mut started = false;
    loop {
        let live = hosts::live_until(paths, deadline).map_err(verify)?;
        if live.len() > 1 || live.iter().any(|host| host.key != key) {
            return Err(Box::new((
                BridgeUnavailableReason::IncompatibleState,
                hosts::conflict(&live),
            )));
        }
        let holder = hosts::lock_holder(&hosts::home_lock(paths)).map_err(verify)?;
        let stopping = holder
            .as_deref()
            .is_some_and(|holder| holder.split_whitespace().nth(2) == Some("stopping"))
            || live.iter().any(|host| {
                host.holder
                    .as_deref()
                    .is_some_and(|holder| holder.split_whitespace().nth(2) == Some("stopping"))
            });
        if stopping {
            return Err(Box::new((
                BridgeUnavailableReason::Stopping,
                CliFailure::new("the host is stopping; retry; no embedded session was started"),
            )));
        }
        match hosts::socket_until(&socket, deadline) {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => {
                return Err(verify(CliFailure::new(format!(
                    "the host socket could not be verified: {error}"
                ))));
            }
        }
        if !started && live.is_empty() && holder.is_none() {
            if auto_start {
                started = super::starter::start(paths, deadline)
                    .map_err(|error| Box::new((error.reason, *error.failure)))?;
                if started {
                    continue;
                }
            } else {
                return Err(Box::new((
                    BridgeUnavailableReason::Absent,
                    CliFailure::new(
                        "no running host for this home; start plakd host or its user unit; no host or embedded session was started",
                    ),
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(Box::new((
                BridgeUnavailableReason::Starting,
                CliFailure::new(if started {
                    "the auto-started host did not become ready; inspect host/auto-start.log in the public home; no embedded session was started"
                } else {
                    "the host is starting or stopping; retry; no host or embedded session was started"
                }),
            )));
        }
        std::thread::sleep(
            Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn relay(mut stream: UnixStream, hello: &str) -> CliResult<ProcessOutcome> {
    stream.write_all(hello.as_bytes()).map_err(failed)?;
    let mut writer = stream.try_clone().map_err(failed)?;
    // Each direction has one bounded buffer. EOF closes the
    // socket's write half; the host detaches this client, not its session.
    let input = std::thread::spawn(move || {
        let result = forward(&mut io::stdin().lock(), &mut writer);
        let _ = writer.shutdown(Shutdown::Write);
        result
    });
    let result = forward(&mut stream, &mut io::stdout().lock());
    let _ = stream.shutdown(Shutdown::Both);
    result.map_err(failed)?;
    // If the host closed first, stdin may still be waiting for its owner.
    // Do not park process exit on that read; it owns no host/session work.
    if input.is_finished() {
        input
            .join()
            .map_err(|_| CliFailure::new("the bridge input reader failed"))?
            .map_err(failed)?;
    }
    Ok(ProcessOutcome::Success)
}

fn forward(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&buffer[..count])?;
        // The client's next request may wait for this frame; never retain
        // it in stdout until EOF or a later frame fills the buffer.
        writer.flush()?;
    }
}

fn failed(error: io::Error) -> CliFailure {
    CliFailure::new(format!("host bridge disconnected: {error}"))
}
