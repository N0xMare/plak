#![allow(
    clippy::unwrap_used,
    reason = "private directory and kernel-lock fixtures"
)]

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::UnixListener;

use super::*;

const OLD: &str = "r00.e00.p00.t00";

fn private(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn paths(root: &Path) -> PlakPaths {
    let paths = PlakPaths::explicit(root.join("home"));
    paths.ensure_layout().unwrap();
    private(&paths.host_run_dir());
    paths
}

fn held(paths: &PlakPaths, key: &str, pid: u32) -> File {
    let directory = paths.state_root().join(key);
    private(&directory);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(directory.join("host.lock"))
        .unwrap();
    writeln!(file, "{pid} fixture-binary").unwrap();
    file.try_lock().unwrap();
    file
}

#[test]
fn a_kernel_held_other_key_with_no_socket_is_live_even_with_a_stale_pid() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let _held = held(&paths, OLD, u32::MAX);
    let hosts = live(&paths).unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].key, OLD);
    assert!(hosts[0].holder.as_ref().unwrap().contains("fixture-binary"));
    assert!(conflict(&hosts).message().contains(OLD));
}

#[test]
fn a_free_lock_naming_a_live_pid_and_a_stale_socket_do_not_prove_a_host() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    drop(held(&paths, OLD, std::process::id()));
    drop(UnixListener::bind(paths.host_run_dir().join(format!("{OLD}.sock"))).unwrap());
    assert_eq!(live(&paths).unwrap(), []);
}

#[test]
fn every_live_socket_key_is_reported_without_selecting_or_stopping_one() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let _first = UnixListener::bind(paths.host_run_dir().join("first.sock")).unwrap();
    let _second = UnixListener::bind(paths.host_run_dir().join("second.sock")).unwrap();
    let hosts = live(&paths).unwrap();
    assert_eq!(
        hosts
            .iter()
            .map(|host| host.key.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    let message = conflict(&hosts).message().to_owned();
    assert!(message.contains("first") && message.contains("second"));
}

#[test]
fn unsafe_lock_or_discovery_directory_is_not_treated_as_absence() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let _held = held(&paths, OLD, std::process::id());
    let lock = paths.state_root().join(OLD).join("host.lock");
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(live(&paths).is_err());
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(paths.host_run_dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(live(&paths).is_err());
}

#[test]
fn a_state_directory_symlink_is_not_silently_skipped() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    std::os::unix::fs::symlink(root.path(), paths.state_root().join(OLD)).unwrap();
    assert!(live(&paths).is_err());
}

#[test]
fn a_full_listener_backlog_is_bounded_and_never_treated_as_absence() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let path = paths.host_run_dir().join("blocked.sock");
    let listener = UnixListener::bind(&path).unwrap();
    rustix::net::listen(&listener, 1).unwrap();
    // A live host holds its state key's kernel lock while it serves.
    let _lock = held(&paths, "blocked", std::process::id());
    let mut clients = Vec::new();
    let mut saturated = false;
    for _ in 0..8 {
        match socket_until(&path, Instant::now() + Duration::from_millis(50)) {
            Ok(client) => clients.push(client),
            Err(error) => {
                // Linux parks a connect on a full backlog until the deadline;
                // macOS refuses it at once, like a stale socket.
                assert!(matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::ConnectionRefused
                ));
                saturated = true;
                break;
            }
        }
    }
    assert!(
        saturated,
        "the unaccepted listener did not fill its backlog"
    );
    // Either the deadline refuses, or the held lock still proves the host.
    match live_until(&paths, Instant::now() + Duration::from_millis(25)) {
        Err(_) => {}
        Ok(hosts) => assert!(
            hosts.iter().any(|host| host.key == "blocked"),
            "a saturated host was treated as absent"
        ),
    }
}
