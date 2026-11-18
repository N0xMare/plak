//! Loopback upstream the relay server core dials through the test-only override.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use super::UpstreamReply;

pub(super) struct Upstream {
    pub(super) requests: Mutex<Vec<Vec<u8>>>,
    pub(super) accepted: AtomicU64,
    pub(super) closes: AtomicU64,
    pub(super) closed: Notify,
    scripts: Mutex<VecDeque<Script>>,
}

enum Script {
    Reply(Vec<u8>),
    /// Write this prefix, then wait until the caller drops the connection.
    Hang(Vec<u8>),
}

impl Upstream {
    pub(super) fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            accepted: AtomicU64::new(0),
            closes: AtomicU64::new(0),
            closed: Notify::new(),
            scripts: Mutex::new(VecDeque::new()),
        }
    }

    pub(super) fn push_reply(&self, bytes: Vec<u8>) {
        self.push(Script::Reply(bytes));
    }

    pub(super) fn push_hang(&self, prefix: Vec<u8>) {
        self.push(Script::Hang(prefix));
    }

    fn push(&self, script: Script) {
        lock(&self.scripts).push_back(script);
    }

    pub(super) async fn wait_for_close(&self) {
        let start = self.closes.load(Ordering::SeqCst);
        loop {
            if self.closes.load(Ordering::SeqCst) > start {
                return;
            }
            self.closed.notified().await;
        }
    }
}

pub(super) fn encode_reply(reply: &UpstreamReply) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {} OK\r\n", reply.status).into_bytes();
    for header in &reply.headers {
        out.extend_from_slice(header.name().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(header.value());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(
        format!(
            "content-length: {}\r\nconnection: close\r\n\r\n",
            reply.body.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&reply.body);
    out
}

pub(super) fn hang_after_head() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 64\r\nconnection: close\r\n\r\n"
        .to_vec()
}

pub(super) async fn serve(listener: TcpListener, upstream: Arc<Upstream>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        upstream.accepted.fetch_add(1, Ordering::SeqCst);
        let upstream = Arc::clone(&upstream);
        tokio::spawn(async move {
            let request = read_http(&mut socket).await;
            lock(&upstream.requests).push(request);
            let script = lock(&upstream.scripts)
                .pop_front()
                .unwrap_or_else(|| Script::Reply(encode_reply(&UpstreamReply::default())));
            match script {
                Script::Reply(bytes) => {
                    let _ = socket.write_all(&bytes).await;
                    let _ = socket.shutdown().await;
                }
                Script::Hang(prefix) => {
                    let _ = socket.write_all(&prefix).await;
                    let mut buffer = [0_u8; 16];
                    while matches!(socket.read(&mut buffer).await, Ok(read) if read > 0) {}
                    upstream.closes.fetch_add(1, Ordering::SeqCst);
                    upstream.closed.notify_one();
                }
            }
        });
    }
}

async fn read_http(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        if complete(&request) {
            return request;
        }
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => return request,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
}

fn complete(bytes: &[u8]) -> bool {
    let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    bytes.len() >= end + 4 + length
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
