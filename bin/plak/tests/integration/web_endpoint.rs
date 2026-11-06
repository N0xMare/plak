//! `plak web` end to end: the real binary on a private unix socket, the
//! scripted offline model behind it, and a WebSocket client that comes in
//! the way `tailscale serve` would, with the tailnet identity header.
//!
//! Every wait here is for an event (a line on stderr, a frame, a closed
//! socket) bounded by the fixture's liveness deadline; nothing asserts on
//! elapsed time.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private real-process test harness"
)]

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::mpsc;

use plakous_host_protocol::wire::{ClientFrame, Hello, NodeAddress, Op, WIRE_VERSION, encode_line};
use plakous_host_protocol::{Core, Draft, SessionIntent};
use serde_json::{Value, json};
use tungstenite::protocol::{Role, WebSocketConfig};
use tungstenite::{Message, WebSocket};

use super::offline_support::*;

const LOGIN: &str = "a@example.com";
const HOST: &str = "plak.example.ts.net";
const ORIGIN: &str = "https://plak.example.ts.net";
const ANSWER: &str = "the web answer";
/// A device of the tailnet that is not this machine (a documentation
/// address, never assigned).
const DEVICE: &str = "192.0.2.10";
const CSP: &str = "default-src 'self'; frame-ancestors 'none'";

/// A running `plak web`.
struct Web {
    child: Option<Child>,
    socket: PathBuf,
    /// Its stderr lines after the start line.
    lines: mpsc::Receiver<String>,
}

impl Drop for Web {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A socket directory short enough for a unix socket path.
fn socket_dir(fixture: &Fixture) -> PathBuf {
    let preferred = fixture.root.join("web");
    if preferred.join("plak.sock").as_os_str().len() < 100 {
        return preferred;
    }
    std::env::temp_dir().join(format!("pw-{}", std::process::id()))
}

fn start(fixture: &Fixture) -> Web {
    let (web, ready) = launch(fixture);
    assert!(ready, "plak web started");
    web
}

/// Starts `plak web` on the fixture's socket; `true` once it serves,
/// `false` when it exited first.
fn launch(fixture: &Fixture) -> (Web, bool) {
    launch_with(fixture, &[])
}

/// [`launch`] with `extra` arguments to `plak web`.
fn launch_with(fixture: &Fixture, extra: &[&str]) -> (Web, bool) {
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
    std::fs::write(ui.join("csp.json"), format!(r#"{{"header": "{CSP}"}}"#)).unwrap();
    std::fs::write(ui.join("index.html"), "<!doctype html>web").unwrap();
    let socket = socket_dir(fixture).join("plak.sock");
    let mut command = fixture.command();
    command
        .arg("web")
        .arg("--listen")
        .arg(format!("unix:{}", socket.display()))
        .arg("--ui")
        .arg(&ui)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("plak web spawned");
    let stderr = child.stderr.take().unwrap();
    let (ready, started) = mpsc::channel();
    let (forward, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.starts_with("plak web: serving on ") {
                let _ = ready.send(true);
            } else {
                let _ = forward.send(line);
            }
        }
        let _ = ready.send(false);
    });
    let served = started
        .recv_timeout(WAIT)
        .expect("plak web started or exited");
    (
        Web {
            child: Some(child),
            socket,
            lines,
        },
        served,
    )
}

/// A response head: the status and the headers, names lower-cased.
struct Head {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Sends one request head and reads the response head byte by byte, so
/// nothing after it is consumed.
fn exchange(stream: &mut UnixStream, method: &str, path: &str, headers: &[(&str, String)]) -> Head {
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8(head).unwrap();
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Head { status, headers }
}

fn connect(web: &Web) -> UnixStream {
    let stream = UnixStream::connect(&web.socket).unwrap();
    // A read the endpoint never answers fails the test rather than hang it.
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    stream
}

/// The headers `tailscale serve` sends a unix backend for a request from
/// `source` to `forwarded_host`: `Host: localhost`, the browser's host in
/// `X-Forwarded-Host`, and the tailnet source address.
fn serve_headers(forwarded_host: &str, source: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Host", "localhost".to_owned()),
        ("X-Forwarded-Host", forwarded_host.to_owned()),
        ("X-Forwarded-Proto", "https".to_owned()),
        ("X-Forwarded-For", source.to_owned()),
    ]
}

/// `GET /` as Serve forwards it for `login`, from another device.
fn page(web: &Web, login: Option<&str>, host: &str) -> Head {
    let mut headers = serve_headers(host, DEVICE);
    headers.push(("Connection", "close".to_owned()));
    if let Some(login) = login {
        headers.push(("Tailscale-User-Login", login.to_owned()));
    }
    exchange(&mut connect(web), "GET", "/", &headers)
}

/// The session cookie a page load issues.
fn cookie(web: &Web) -> String {
    let head = page(web, Some(LOGIN), HOST);
    assert_eq!(head.status, 200);
    let set = head.header("set-cookie").expect("a session cookie");
    for attribute in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
        assert!(set.contains(attribute), "{set}");
    }
    assert!(!set.contains("Domain"), "{set}");
    set.split(';').next().unwrap().to_owned()
}

/// The upgrade request headers, each overridable.
fn upgrade_headers(origin: Option<&str>, cookie: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = serve_headers(HOST, DEVICE);
    headers.extend([
        ("Tailscale-User-Login", LOGIN.to_owned()),
        ("Upgrade", "websocket".to_owned()),
        ("Connection", "Upgrade".to_owned()),
        ("Sec-WebSocket-Version", "13".to_owned()),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==".to_owned()),
    ]);
    if let Some(origin) = origin {
        headers.push(("Origin", origin.to_owned()));
    }
    if let Some(cookie) = cookie {
        headers.push(("Cookie", cookie.to_owned()));
    }
    headers
}

fn upgrade(web: &Web, cookie: &str) -> WebSocket<UnixStream> {
    let mut stream = connect(web);
    let head = exchange(
        &mut stream,
        "GET",
        "/wire",
        &upgrade_headers(Some(ORIGIN), Some(cookie)),
    );
    assert_eq!(head.status, 101);
    assert_eq!(
        head.header("sec-websocket-accept"),
        Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
    );
    let config = WebSocketConfig::default().max_message_size(Some(64 << 20));
    WebSocket::from_raw_socket(stream, Role::Client, Some(config))
}

fn send(socket: &mut WebSocket<UnixStream>, frame: &ClientFrame<Core>) {
    let line = encode_line(frame).unwrap();
    socket.send(Message::text(line)).unwrap();
}

/// The next host frame, as JSON.
fn next_frame(socket: &mut WebSocket<UnixStream>) -> Value {
    loop {
        match socket.read().expect("a host frame") {
            Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected message {other:?}"),
        }
    }
}

/// The handshake; the welcome names the session's root node.
fn hello(socket: &mut WebSocket<UnixStream>) -> NodeAddress {
    send(
        socket,
        &ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "web-test".into(),
            caps: Vec::new(),
        }),
    );
    let welcome = next_frame(socket);
    assert_eq!(welcome["frame"], "welcome", "{welcome}");
    serde_json::from_value(welcome["session"]["root"].clone()).expect("the welcome names the root")
}

/// The response to request `id`, skipping the events before it.
fn response(socket: &mut WebSocket<UnixStream>, id: u64) -> Value {
    loop {
        let frame = next_frame(socket);
        if frame["frame"] == "response" && frame["id"] == id {
            return frame["outcome"].clone();
        }
    }
}

fn attach(socket: &mut WebSocket<UnixStream>, root: &NodeAddress, id: u64) -> u64 {
    send(
        socket,
        &ClientFrame::Request {
            id,
            op: Op::Attach { node: root.clone() },
        },
    );
    let outcome = response(socket, id);
    assert_eq!(outcome["ok"]["answer"], "attached", "{outcome}");
    outcome["ok"]["attachment"]
        .as_u64()
        .unwrap_or_else(|| panic!("attached: {outcome}"))
}

fn interrupt_and_wait(web: &mut Web) {
    let killed = std::process::Command::new("kill")
        .args(["-INT", &web.child.as_ref().unwrap().id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let (done, exited) = mpsc::channel();
    let mut child = web.child.take().unwrap();
    std::thread::spawn(move || {
        let _ = done.send(child.wait());
    });
    let status = exited.recv_timeout(WAIT).expect("plak web exited").unwrap();
    assert!(status.success(), "{status}");
}

/// A client with the identity and the cookie attaches to the session's
/// root, prompts, and reads the scripted answer as events; the session
/// survives its leaving, and an interrupt ends the process cleanly.
#[test]
fn a_tailnet_client_attaches_prompts_and_reads_the_answer() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": ANSWER}}]
    }));
    let mut web = start(&fixture);
    let head = page(&web, Some(LOGIN), HOST);
    assert_eq!(head.header("content-security-policy"), Some(CSP));
    assert_eq!(head.header("x-content-type-options"), Some("nosniff"));
    let cookie = cookie(&web);
    let mut socket = upgrade(&web, &cookie);
    let root = hello(&mut socket);
    let attachment = attach(&mut socket, &root, 1);
    send(
        &mut socket,
        &ClientFrame::Request {
            id: 2,
            op: Op::Intent {
                attachment,
                intent: SessionIntent::Prompt(Draft::from("hello from the web")),
                blobs: Vec::new(),
            },
        },
    );
    assert_eq!(
        response(&mut socket, 2),
        json!({"ok": {"answer": "accepted"}})
    );
    loop {
        let frame = next_frame(&mut socket);
        if frame["frame"] == "event" && frame.to_string().contains(ANSWER) {
            break;
        }
    }
    socket.close(None).unwrap();
    drop(socket);
    // The session runs on for the next connection.
    let mut again = upgrade(&web, &cookie);
    let root = hello(&mut again);
    attach(&mut again, &root, 1);
    drop(again);
    interrupt_and_wait(&mut web);
    assert!(!web.socket.exists(), "the socket is removed on exit");
}

/// Everything that fails a check gets the same 403: no identity, a login
/// not on the list, another forwarded host, a request in any shape but
/// Serve's (sent to the socket directly, or with no or a local source), an
/// absolute-form target, an upgrade from another origin or without the
/// cookie, a forged cookie. A non-GET method, and a HEAD upgrade, are 405,
/// and nothing answers with CORS headers.
#[test]
fn every_failed_check_is_refused_alike() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": [{"output": {"text": ANSWER}}]}));
    let web = start(&fixture);
    let cookie = cookie(&web);
    let refused = [
        page(&web, None, HOST),
        page(&web, Some("eve@example.com"), HOST),
        page(&web, Some(LOGIN), "evil.example"),
        exchange(
            &mut connect(&web),
            "GET",
            "/wire",
            &upgrade_headers(Some("https://evil.example"), Some(&cookie)),
        ),
        exchange(
            &mut connect(&web),
            "GET",
            "/wire",
            &upgrade_headers(None, Some(&cookie)),
        ),
        exchange(
            &mut connect(&web),
            "GET",
            "/wire",
            &upgrade_headers(Some(ORIGIN), None),
        ),
        exchange(
            &mut connect(&web),
            "GET",
            "/wire",
            &upgrade_headers(Some(ORIGIN), Some("__Host-plak=forged.0.x.y")),
        ),
        // A client's own shape: the exact Host and no forwarding, which
        // Serve never sends a unix backend.
        exchange(
            &mut connect(&web),
            "GET",
            "/",
            &[
                ("Host", HOST.to_owned()),
                ("Tailscale-User-Login", LOGIN.to_owned()),
                ("Connection", "close".to_owned()),
            ],
        ),
        // Serve's shape from the serving machine itself, or with no source.
        exchange(&mut connect(&web), "GET", "/", &{
            let mut headers = serve_headers(HOST, "127.0.0.1");
            headers.push(("Tailscale-User-Login", LOGIN.to_owned()));
            headers.push(("Connection", "close".to_owned()));
            headers
        }),
        exchange(&mut connect(&web), "GET", "/", &{
            let mut headers = serve_headers(HOST, DEVICE);
            headers.retain(|(name, _)| *name != "X-Forwarded-For");
            headers.push(("Tailscale-User-Login", LOGIN.to_owned()));
            headers.push(("Connection", "close".to_owned()));
            headers
        }),
        exchange(&mut connect(&web), "GET", "http://evil.example/", &{
            let mut headers = serve_headers(HOST, DEVICE);
            headers.push(("Tailscale-User-Login", LOGIN.to_owned()));
            headers.push(("Connection", "close".to_owned()));
            headers
        }),
    ];
    for head in &refused {
        assert_eq!(head.status, 403);
        assert!(head.header("set-cookie").is_none());
        assert!(
            head.headers
                .iter()
                .all(|(name, _)| !name.starts_with("access-control-"))
        );
    }
    let post = exchange(
        &mut connect(&web),
        "POST",
        "/",
        &[
            ("Host", HOST.to_owned()),
            ("Tailscale-User-Login", LOGIN.to_owned()),
            ("Content-Length", "0".to_owned()),
        ],
    );
    assert_eq!(post.status, 405);
    let head_upgrade = exchange(
        &mut connect(&web),
        "HEAD",
        "/wire",
        &upgrade_headers(Some(ORIGIN), Some(&cookie)),
    );
    assert_eq!(head_upgrade.status, 405);
}

/// A second endpoint on a live socket refuses to start, and the first
/// keeps serving at its path.
#[test]
fn a_live_socket_is_never_replaced() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": [{"output": {"text": ANSWER}}]}));
    let web = start(&fixture);
    let (mut second, served) = launch(&fixture);
    assert!(!served, "a second endpoint started on a live socket");
    let status = second.child.take().unwrap().wait().unwrap();
    assert!(!status.success());
    assert!(web.socket.exists());
    assert_eq!(page(&web, Some(LOGIN), HOST).status, 200);
}

/// A message past the 16 MiB frame cap ends that connection with no answer;
/// the session itself is untouched and takes the next one.
#[test]
fn an_oversized_frame_ends_only_its_connection() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": [{"output": {"text": ANSWER}}]}));
    let web = start(&fixture);
    let cookie = cookie(&web);
    let mut socket = upgrade(&web, &cookie);
    let root = hello(&mut socket);
    attach(&mut socket, &root, 1);
    let oversized = "x".repeat(16 * 1024 * 1024 + 1);
    // The host may close while the message is still being written.
    let _ = socket.send(Message::text(oversized));
    loop {
        match socket.read() {
            Ok(Message::Close(frame)) => {
                if let Some(frame) = frame {
                    assert_eq!(u16::from(frame.code), 1008);
                }
                break;
            }
            Ok(Message::Text(text)) => {
                let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                assert_ne!(frame["frame"], "response", "{frame}");
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let mut again = upgrade(&web, &cookie);
    let root = hello(&mut again);
    attach(&mut again, &root, 1);
}

/// A session nothing keeps resident releases itself after the idle
/// release, and the next connection opens the same session again: its
/// welcome names that session, and it takes the next prompt.
#[test]
fn an_idle_session_releases_and_the_next_connection_wakes_it() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": ANSWER}}, {"output": {"text": ANSWER}}]
    }));
    let (mut web, ready) = launch_with(&fixture, &["--idle-release", "1"]);
    assert!(ready, "plak web started");
    let woke = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake(&mut web)));
    if let Err(error) = woke {
        let lines: Vec<String> = web.lines.try_iter().collect();
        let text = error
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| error.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        panic!("{text}; plak web said: {lines:#?}");
    }
    interrupt_and_wait(&mut web);
}

/// The wake test's body: a prompt, a release, and a wake that takes the
/// next prompt.
fn wake(web: &mut Web) {
    let cookie = cookie(web);
    let prompt = |socket: &mut WebSocket<UnixStream>, attachment: u64, text: &str, answer: &str| {
        send(
            socket,
            &ClientFrame::Request {
                id: 2,
                op: Op::Intent {
                    attachment,
                    intent: SessionIntent::Prompt(Draft::from(text)),
                    blobs: Vec::new(),
                },
            },
        );
        assert_eq!(response(socket, 2), json!({"ok": {"answer": "accepted"}}));
        loop {
            let frame = next_frame(socket);
            if frame["frame"] == "event" && frame.to_string().contains(answer) {
                break;
            }
        }
    };
    let mut socket = upgrade(web, &cookie);
    let root = hello(&mut socket);
    let attachment = attach(&mut socket, &root, 1);
    prompt(&mut socket, attachment, "before the release", ANSWER);
    socket.close(None).unwrap();
    drop(socket);
    // Released once idle, the session is opened again by the next
    // connection, which says so; until then a connection only waits.
    let mut woken = None;
    let deadline = std::time::Instant::now() + WAIT;
    while woken.is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut again = upgrade(web, &cookie);
        let root_again = hello(&mut again);
        assert_eq!(root_again, root, "the same session is served");
        if web
            .lines
            .try_iter()
            .any(|line| line.starts_with("plak web: opening released session "))
        {
            woken = Some((again, root_again));
        }
    }
    let (mut again, root_again) = woken.expect("the released session was opened again");
    let attachment = attach(&mut again, &root_again, 1);
    // The scripted model behind the reopened session answers as before.
    prompt(&mut again, attachment, "after the wake", ANSWER);
    drop(again);
}

/// A connection holds its session resident from the moment the endpoint
/// hands it the session, before it attaches: a client that waits past the
/// idle release between its welcome and its attach still attaches the
/// session it was handed, which a release would have refused it
/// (`node_retiring`) or ended under it.
#[test]
fn a_connection_holds_its_session_before_it_attaches() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": [{"output": {"text": ANSWER}}]}));
    let (web, ready) = launch_with(&fixture, &["--idle-release", "1"]);
    assert!(ready, "plak web started");
    let cookie = cookie(&web);
    let mut socket = upgrade(&web, &cookie);
    let root = hello(&mut socket);
    // Several idle releases' worth of decisions pass before the attach.
    std::thread::sleep(std::time::Duration::from_secs(4));
    attach(&mut socket, &root, 1);
}
