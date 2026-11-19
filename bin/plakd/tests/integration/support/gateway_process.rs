//! Separate real-binary gateway for offline process contracts.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "offline process fixture"
)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(super) struct GatewayProcess(Child);

impl GatewayProcess {
    /// Give a caller and service an explicit shared socket, independent of the
    /// ambient runtime directory and of long product home paths.
    pub(super) fn configured(command: &mut Command, home: &Path) -> (Self, tempfile::TempDir) {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let socket_dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(socket_dir.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::create_dir_all(home).unwrap();
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = socket_dir.path().join("g.sock");
        std::fs::write(
            home.join("gateway.toml"),
            format!("socket = {:?}\n", socket.to_str().unwrap()),
        )
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join("config.toml"))
            .unwrap();
        writeln!(
            file,
            "\n[gateway]\naddress = {:?}",
            format!("unix:{}", socket.display())
        )
        .unwrap();
        let gateway = Self::start(command, &socket, &socket_dir.path().join("log"));
        (gateway, socket_dir)
    }

    pub(super) fn start(command: &mut Command, socket: &Path, log: &Path) -> Self {
        let output = std::fs::File::create(log).expect("gateway log");
        let mut process = Self(
            command
                .args(["gateway", "serve"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(output)
                .spawn()
                .expect("gateway child"),
        );
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) {
                use std::io::{Read as _, Write as _};
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                if stream
                    .write_all(
                        b"GET /versions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .is_ok()
                {
                    let mut head = [0; 12];
                    if stream.read_exact(&mut head).is_ok() && &head == b"HTTP/1.1 200" {
                        return process;
                    }
                }
            }
            assert!(
                process.0.try_wait().expect("gateway status").is_none() && Instant::now() < until,
                "gateway did not answer: {}",
                std::fs::read_to_string(log).unwrap_or_default(),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn wait_for_drain(&mut self) {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "gateway drain exited {status}");
                return;
            }
            assert!(Instant::now() < until, "gateway did not drain");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
