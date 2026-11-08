//! Test-only transparent recording of the real terminal's control traffic.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::Shutdown;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Clone, Default)]
pub(super) struct Recording(Arc<Mutex<Vec<Value>>>, Arc<AtomicU64>);

impl Recording {
    pub(super) fn is_recording(&self) -> bool {
        self.1.load(Ordering::Acquire) != 0
    }

    pub(super) fn mark(&self, barrier: &str) {
        self.push(json!({"barrier": barrier}));
    }

    fn push(&self, entry: Value) {
        self.0.lock().unwrap().push(entry);
    }

    pub(super) fn entries(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }
}

pub(super) struct Proxy {
    path: PathBuf,
    upstream: PathBuf,
    inode: (u64, u64),
    stopped: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<UnixStream>>>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    listener: Option<JoinHandle<()>>,
}

impl Proxy {
    pub(super) fn start(path: &Path, recording: Recording) -> Self {
        let upstream = path.with_extension("u");
        assert!(!upstream.exists(), "recording path is already occupied");
        std::fs::rename(path, &upstream).unwrap();
        let listener = UnixListener::bind(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let metadata = std::fs::symlink_metadata(path).unwrap();
        let mut proxy = Self {
            path: path.to_path_buf(),
            upstream: upstream.clone(),
            inode: (metadata.dev(), metadata.ino()),
            stopped: Arc::new(AtomicBool::new(false)),
            sockets: Arc::default(),
            workers: Arc::default(),
            listener: None,
        };
        let stopped = Arc::clone(&proxy.stopped);
        let sockets = Arc::clone(&proxy.sockets);
        let workers = Arc::clone(&proxy.workers);
        proxy.listener = Some(std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let server = UnixStream::connect(&upstream).unwrap();
                        let connection = recording.1.fetch_add(1, Ordering::Relaxed) + 1;
                        recording.push(json!({"connection": connection, "opened": true}));
                        sockets
                            .lock()
                            .unwrap()
                            .extend([client.try_clone().unwrap(), server.try_clone().unwrap()]);
                        let client_read = client.try_clone().unwrap();
                        let server_read = server.try_clone().unwrap();
                        let incoming = recording.clone();
                        workers.lock().unwrap().push(std::thread::spawn(move || {
                            forward(client_read, server, &incoming, connection, true);
                        }));
                        let outgoing = recording.clone();
                        workers.lock().unwrap().push(std::thread::spawn(move || {
                            forward(server_read, client, &outgoing, connection, false);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("recording accept: {error}"),
                }
            }
        }));
        proxy
    }
}

fn forward(
    read: UnixStream,
    mut write: UnixStream,
    recording: &Recording,
    connection: u64,
    client: bool,
) {
    let mut reader = BufReader::new(read);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                recording.push(json!({"connection": connection, "eof": if client { "client" } else { "host" }}));
                let _ = write.shutdown(Shutdown::Write);
                return;
            }
            Ok(_) => {
                let frame: Value = serde_json::from_str(&line).unwrap();
                // The complete raw frames remain in the recording artifact.
                // Canonical control traces select requests, acknowledgements
                // and the real driver's explicit semantic barriers later.
                recording.push(json!({"connection": connection, "direction": if client { "client" } else { "host" }, "frame": frame}));
                if write.write_all(line.as_bytes()).is_err() {
                    return;
                }
            }
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // Stop admission first, then every reader/writer; no socket can be
        // registered after this shutdown pass.
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        for socket in self.sockets.lock().unwrap().iter() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        for worker in self.workers.lock().unwrap().drain(..) {
            let _ = worker.join();
        }
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.inode)
        {
            let _ = std::fs::rename(&self.upstream, &self.path);
        }
    }
}
