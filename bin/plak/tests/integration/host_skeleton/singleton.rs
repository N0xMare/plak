//! Direct host starts respect live older keys and the cross-key startup guard.

use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use super::*;

fn private(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn held(path: &std::path::Path) -> std::fs::File {
    private(path.parent().unwrap());
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    writeln!(file, "{} fixture-build", std::process::id()).unwrap();
    file.try_lock().unwrap();
    file
}

#[test]
fn a_direct_start_refuses_a_kernel_held_other_key_before_starting_any_session() {
    let Some(fixture) = fixture() else { return };
    let key = "r00.e00.p00.t00";
    let _held = held(&fixture.home().join("state").join(key).join("host.lock"));
    let (mut child, lines) = spawn(&fixture);
    assert!(!child.wait().unwrap().success());
    let lines = lines.iter().collect::<Vec<_>>().join("\n");
    assert!(
        lines.contains(key) && lines.contains("no host was stopped"),
        "{lines}"
    );
    assert!(!lines.contains("plak host: serving on "));
    assert!(!lines.contains("plak host: opening session "));
}

#[test]
fn an_opt_in_bridge_refuses_another_key_instead_of_starting_a_sibling() {
    let Some(fixture) = fixture() else { return };
    let key = "r00.e00.p00.t00";
    let _held = held(&fixture.home().join("state").join(key).join("host.lock"));
    let output = fixture
        .command()
        .args(["host", "connect", "--stdio", "--auto-start"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains(key) && message.contains("no host was stopped"),
        "{message}"
    );
    assert!(
        output.stdout.is_empty(),
        "diagnostics reached protocol stdout"
    );
    assert!(!fixture.home().join("host/auto-start.log").exists());
}

#[test]
fn a_direct_start_does_not_treat_a_home_guard_without_a_socket_as_absence() {
    let Some(fixture) = fixture() else { return };
    let _held = held(&fixture.home().join("host/host.lock"));
    let (mut child, lines) = spawn(&fixture);
    assert!(!child.wait().unwrap().success());
    let lines = lines.iter().collect::<Vec<_>>().join("\n");
    assert!(lines.contains("already running"), "{lines}");
    assert!(!lines.contains("plak host: serving on "));
}

#[test]
fn a_free_older_key_lock_naming_a_live_pid_does_not_block_a_direct_start() {
    let Some(fixture) = fixture() else { return };
    drop(held(
        &fixture.home().join("state/r00.e00.p00.t00/host.lock"),
    ));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    assert_eq!(client.rows(1), []);
    drop(client);
    stop(&mut host);
}
