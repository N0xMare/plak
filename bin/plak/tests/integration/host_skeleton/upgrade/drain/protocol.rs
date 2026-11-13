//! An older coordinator identity refuses before sending a drain request.

use super::*;
use std::io::Read as _;

#[test]
fn host_step_drain_older_coordinator_refuses_by_identity_before_request_or_pause() {
    let Some(fixture) = fixture() else { return };
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let id = binary(&fixture);
    let path = fixture.home().join("host/upgrade.sock");
    let retained = fixture.home().join("host/retained-upgrade.sock");
    std::fs::rename(&path, &retained).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        writeln!(
            stream,
            "{}",
            json!({"protocol":2,"key":"unused","instance":"unused","binary":"000000000000"})
        )
        .unwrap();
        let mut byte = [0];
        assert_eq!(
            stream.read(&mut byte).unwrap(),
            0,
            "the destination sent an unsupported source a request"
        );
    });
    let refused = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &id, "--drain", "idle"],
        false,
        &[("PLAK_TEST_FORCE_HOST_UPGRADE", "1")],
    );
    worker.join().unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(retained, path).unwrap();
    assert!(!refused.status.success());
    let detail = String::from_utf8_lossy(&refused.stderr);
    assert!(
        detail.contains("host_upgrade_unsupported_source")
            && detail.contains("retained source build"),
        "{detail}"
    );
    assert_eq!(client.request(10, Op::Ping), Outcome::Ok(Answer::Pong));
    assert!(matches!(
        client.request(11, fresh_session(&fixture)),
        Outcome::Ok(Answer::Created { .. })
    ));
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut host);
}
