//! The sweep's `plak web` face: the real endpoint on a private unix socket,
//! entered through its gate as `tailscale serve` would (the tailnet login,
//! the exact host and origin, the issued cookie), and the wire v2 client
//! (`plakous_host_protocol::client`) over the WebSocket, one wire line per
//! text message.
//!
//! A cut process ends wherever its commit lands, so every step here either
//! completes or finds the process gone and stops; a step that fails while
//! the process lives is the shape failing.

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use plak::wire_transport::PlakExtension;
use plakous_host_protocol::client::{self, Transport, WireReceiver, WireSender};
use plakous_host_protocol::wire::{Hello, WIRE_VERSION};
use plakous_host_protocol::{Draft, SessionIntent, SessionObservation};
use serde_json::{Value, json};
use tungstenite::protocol::{Role, WebSocketConfig};
use tungstenite::{Message, WebSocket};

use super::super::offline_support::{AgentOutcome, Fixture, WAIT, write_private};
use super::shapes::ANSWER;

const LOGIN: &str = "sweep@example.com";
const HOST: &str = "plak.example.ts.net";
const ORIGIN: &str = "https://plak.example.ts.net";
/// A device of the tailnet that is not this machine (a documentation
/// address, never assigned).
const DEVICE: &str = "192.0.2.10";

/// Adds the `[web]` keys and the UI directory `plak web` serves; repeatable.
pub(super) fn configure(fixture: &Fixture) {
    let config = fixture.home().join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    if !text.contains("\n[web]\n") {
        let _ = write!(
            text,
            "\n[web]\nallow = [\"{LOGIN}\"]\norigin = \"{ORIGIN}\"\n"
        );
        write_private(&config, &text);
    }
    let ui = fixture.root.join("ui");
    std::fs::create_dir_all(&ui).unwrap();
    std::fs::write(
        ui.join("csp.json"),
        r#"{"header": "default-src 'self'; frame-ancestors 'none'"}"#,
    )
    .unwrap();
    std::fs::write(ui.join("index.html"), "<!doctype html>sweep").unwrap();
}

/// The dormant shape's lifecycle on `plak web --session <session>`, which
/// releases its session one second after nothing keeps it: a connection
/// prompts and reads the answer, then leaves; once the session's runtime
/// lease is released, the next connection wakes it, and an interrupt ends
/// the process while that connection holds the woken session.
pub(super) fn dormant_lifecycle(
    fixture: &Fixture,
    command: Command,
    session: &str,
) -> AgentOutcome {
    let mut web = Web::start(fixture, command, session);
    let mut records = Vec::new();
    let _ = dormant_steps(fixture, &mut web, session, &mut records);
    web.finish(&records)
}

fn dormant_steps(
    fixture: &Fixture,
    web: &mut Web,
    session: &str,
    records: &mut Vec<Value>,
) -> Option<()> {
    web.serving()?;
    let first = web.connect(session)?;
    if first
        .sender
        .send(SessionIntent::Prompt(Draft::from(
            "crash-sweep-original-prompt",
        )))
        .is_err()
    {
        return web.lost("refused the prompt");
    }
    first.answer(web)?;
    records.push(json!({"kind": "assistant", "text": ANSWER}));
    if !holds_runtime_lease(fixture) {
        return web.lost("served a session without its runtime lease");
    }
    drop(first);
    web.released(fixture)?;
    records.push(json!({"kind": "released"}));
    let woken = web.connect(session)?;
    woken.attached(web)?;
    if !web.said("plak web: opened session ") {
        return web.lost("served a released session without opening it");
    }
    records.push(json!({"kind": "woke"}));
    web.interrupt();
    let exited = web.exited();
    drop(woken);
    exited
}

/// Whether the runtime store has a branch with a live owner.
fn holds_runtime_lease(fixture: &Fixture) -> bool {
    let database = super::summary::paths(fixture).runtime_database();
    let Ok(connection) =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return false;
    };
    connection
        .query_row(
            "SELECT COUNT(*) FROM branches WHERE active_owner IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .is_ok_and(|owners| owners > 0)
}

/// A running `plak web`.
struct Web {
    child: Child,
    socket: PathBuf,
    /// Every stderr line so far.
    stderr: Arc<Mutex<Vec<String>>>,
    serving: mpsc::Receiver<bool>,
    reader: Option<JoinHandle<()>>,
}

impl Web {
    fn start(fixture: &Fixture, mut command: Command, session: &str) -> Self {
        let socket = socket_path(fixture);
        command
            .arg("web")
            .arg("--listen")
            .arg(format!("unix:{}", socket.display()))
            .arg("--ui")
            .arg(fixture.root.join("ui"))
            .args(["--session", session, "--idle-release", "1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("plak web spawned");
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let (ready, serving) = mpsc::channel();
        let reader = {
            let (pipe, stderr) = (child.stderr.take().unwrap(), Arc::clone(&stderr));
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if line.starts_with("plak web: serving on ") {
                        let _ = ready.send(true);
                    }
                    stderr.lock().unwrap().push(line);
                }
                let _ = ready.send(false);
            })
        };
        Self {
            child,
            socket,
            stderr,
            serving,
            reader: Some(reader),
        }
    }

    /// `Some` once the endpoint serves.
    fn serving(&mut self) -> Option<()> {
        match self.serving.recv_timeout(WAIT) {
            Ok(true) => Some(()),
            Ok(false) => self.lost("exited before serving"),
            Err(_) => self.lost("did not serve in time"),
        }
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// `None` when the process ended (a cut); otherwise the shape failed
    /// with `what`. A cut connection fails a moment before its process is
    /// reaped, so the process is given that moment.
    fn lost<T>(&mut self, what: &str) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !self.alive() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "plak web {what}; stderr: {:#?}",
            self.stderr.lock().unwrap()
        );
    }

    fn said(&self, prefix: &str) -> bool {
        self.stderr
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with(prefix))
    }

    /// A connection through the gate, welcomed to `session`.
    fn connect(&mut self, session: &str) -> Option<Connection> {
        match connect(&self.socket) {
            Ok(connection) if connection.session == session => Some(connection),
            Ok(connection) => self.lost(&format!(
                "welcomed a connection to {}, not {session}",
                connection.session
            )),
            Err(error) => self.lost(&format!("refused a connection: {error}")),
        }
    }

    /// `Some` once the runtime store holds no live owner: the session was
    /// released.
    fn released(&mut self, fixture: &Fixture) -> Option<()> {
        let deadline = Instant::now() + WAIT;
        while holds_runtime_lease(fixture) {
            if !self.alive() {
                return None;
            }
            if Instant::now() >= deadline {
                return self.lost("did not release the idle session");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Some(())
    }

    fn interrupt(&self) {
        let pid = nix::unistd::Pid::from_raw(i32::try_from(self.child.id()).unwrap());
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT);
    }

    /// `Some` once the process exited.
    fn exited(&mut self) -> Option<()> {
        let deadline = Instant::now() + WAIT;
        while self.alive() {
            if Instant::now() >= deadline {
                return self.lost("did not exit after the interrupt");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(())
    }

    /// Waits for the process (stopping one that outlived its lifecycle) and
    /// returns its outcome; `records` are the lifecycle's own milestones.
    fn finish(mut self, records: &[Value]) -> AgentOutcome {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let stderr = self.stderr.lock().unwrap().join("\n");
        AgentOutcome {
            status,
            lines: records.iter().map(Value::to_string).collect(),
            stderr,
        }
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A socket path short enough for a unix socket.
fn socket_path(fixture: &Fixture) -> PathBuf {
    let preferred = fixture.root.join("web").join("plak.sock");
    if preferred.as_os_str().len() < 100 {
        return preferred;
    }
    let name = fixture.root.file_name().unwrap().to_string_lossy();
    PathBuf::from(format!("/tmp/pw-{name}")).join("plak.sock")
}

/// One wire client on the endpoint.
struct Connection {
    sender: WireSender<PlakExtension>,
    receiver: WireReceiver<PlakExtension>,
    session: String,
    bridge: Bridge,
}

impl Connection {
    /// `Some` once the scripted answer arrived.
    fn answer(&self, web: &mut Web) -> Option<()> {
        let deadline = Instant::now() + WAIT;
        let mut streamed = String::new();
        loop {
            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(SessionObservation::AssistantDelta(delta)) => streamed.push_str(&delta),
                Ok(observation) if format!("{observation:?}").contains(ANSWER) => return Some(()),
                Err(RecvTimeoutError::Disconnected) => return web.lost("ended before the answer"),
                Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                    return web.lost("gave no answer in time");
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            }
            if streamed.contains(ANSWER) {
                return Some(());
            }
        }
    }

    /// `Some` once the connection's attachment delivered its first
    /// observation: it is attached.
    fn attached(&self, web: &mut Web) -> Option<()> {
        match self.receiver.recv_timeout(WAIT) {
            Ok(_) => Some(()),
            Err(_) => web.lost("never attached the woken session"),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.bridge.close();
    }
}

fn connect(socket: &std::path::Path) -> Result<Connection, String> {
    let cookie = cookie(socket)?;
    let mut stream = stream(socket)?;
    let mut headers = serve_headers();
    headers.extend([
        ("Upgrade", "websocket".to_owned()),
        ("Connection", "Upgrade".to_owned()),
        ("Sec-WebSocket-Version", "13".to_owned()),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==".to_owned()),
        ("Origin", ORIGIN.to_owned()),
        ("Cookie", cookie),
    ]);
    let (status, _) = exchange(&mut stream, "/wire", &headers)?;
    if status != 101 {
        return Err(format!("the upgrade answered {status}"));
    }
    let config = WebSocketConfig::default().max_message_size(Some(64 << 20));
    let socket = WebSocket::from_raw_socket(stream, Role::Client, Some(config));
    let (transport, bridge) = bridge(socket).map_err(|error| error.to_string())?;
    let hello = Hello {
        proto: WIRE_VERSION,
        binary_id: "crash-sweep".into(),
        caps: Vec::new(),
    };
    let (sender, receiver, welcome) =
        client::connect::<PlakExtension>(transport, hello).map_err(|error| error.to_string())?;
    Ok(Connection {
        sender,
        receiver,
        session: welcome
            .session
            .map(|served| served.session_id)
            .unwrap_or_default(),
        bridge,
    })
}

fn stream(socket: &std::path::Path) -> Result<UnixStream, String> {
    let stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|error| error.to_string())?;
    Ok(stream)
}

/// The headers `tailscale serve` sends a unix backend for this login.
fn serve_headers() -> Vec<(&'static str, String)> {
    vec![
        ("Host", "localhost".to_owned()),
        ("X-Forwarded-Host", HOST.to_owned()),
        ("X-Forwarded-Proto", "https".to_owned()),
        ("X-Forwarded-For", DEVICE.to_owned()),
        ("Tailscale-User-Login", LOGIN.to_owned()),
    ]
}

/// The session cookie a page load issues.
fn cookie(socket: &std::path::Path) -> Result<String, String> {
    let mut headers = serve_headers();
    headers.push(("Connection", "close".to_owned()));
    let (status, response) = exchange(&mut stream(socket)?, "/", &headers)?;
    if status != 200 {
        return Err(format!("the page answered {status}"));
    }
    response
        .into_iter()
        .find(|(name, _)| name == "set-cookie")
        .and_then(|(_, value)| value.split(';').next().map(str::to_owned))
        .ok_or_else(|| "the page issued no cookie".to_owned())
}

/// Sends a `GET` head and reads the response head byte by byte, so nothing
/// after it is consumed: the status and the headers, names lower-cased.
fn exchange(
    stream: &mut UnixStream,
    path: &str,
    headers: &[(&str, String)],
) -> Result<(u16, Vec<(String, String)>), String> {
    let mut request = format!("GET {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) => return Err("the endpoint closed before its response".to_owned()),
            Ok(_) => head.push(byte[0]),
            Err(error) => return Err(error.to_string()),
        }
    }
    let text = String::from_utf8(head).map_err(|error| error.to_string())?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("no status line in {text:?}"))?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Ok((status, headers))
}

/// Carries wire lines between a client transport and the WebSocket: one
/// thread owns the socket, reading with a short timeout and sending what
/// the client wrote in between.
struct Bridge {
    closing: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Bridge {
    fn close(&mut self) {
        self.closing.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn bridge(mut socket: WebSocket<UnixStream>) -> std::io::Result<(Transport, Bridge)> {
    let (client, host) = UnixStream::pair()?;
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(10)))?;
    let (lines, written) = mpsc::channel::<String>();
    let mut from_client = BufReader::new(host.try_clone()?);
    std::thread::spawn(move || {
        let mut line = String::new();
        while from_client.read_line(&mut line).is_ok_and(|read| read > 0) {
            if lines.send(line.trim_end().to_owned()).is_err() {
                return;
            }
            line.clear();
        }
    });
    let closing = Arc::new(AtomicBool::new(false));
    let thread = {
        let closing = Arc::clone(&closing);
        let mut to_client = host;
        std::thread::spawn(move || {
            'carry: loop {
                if closing.load(Ordering::Acquire) {
                    let _ = socket.close(None);
                    let _ = socket.flush();
                    break;
                }
                match socket.read() {
                    Ok(Message::Text(text)) => {
                        if writeln!(to_client, "{}", text.as_str()).is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
                loop {
                    match written.try_recv() {
                        Ok(line) => {
                            if socket.send(Message::text(line)).is_err() {
                                break 'carry;
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break 'carry,
                    }
                }
            }
            let _ = to_client.shutdown(Shutdown::Both);
        })
    };
    Ok((
        Transport::new(client.try_clone()?, client),
        Bridge {
            closing,
            thread: Some(thread),
        },
    ))
}
