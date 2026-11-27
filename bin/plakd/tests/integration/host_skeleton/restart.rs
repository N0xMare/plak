//! Restart upgrades over real binaries in isolated homes (spec 02 §4.6): a
//! planned SIGTERM stop, then a start of the same or a replacement image on
//! the same stores and state key.

use std::path::Path;
use std::time::{Duration, Instant};

use super::*;

mod changed;
mod lifecycle;
mod probes;
mod retained_jobs;
mod startup;

pub(super) fn output(fixture: &Fixture, arguments: &[&str]) -> std::process::Output {
    fixture.command().args(arguments).output().unwrap()
}

pub(super) fn seed(fixture: &Fixture, ordinal: usize) -> String {
    let mut document: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/session-export-v13.json")).unwrap();
    document["source"]["session_id"] = json!(uuid::Uuid::new_v4().to_string());
    document["workspace_root"] = json!(fixture.workspace());
    document["named_state"] = json!({"answer": ordinal});
    document["queue"] = json!([format!("paused input {ordinal}")]);
    let path = fixture.root.join(format!("seed-{ordinal}.json"));
    write_private(&path, &document.to_string());
    let result = output(fixture, &["session", "import", path.to_str().unwrap()]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plakd run --session "))
        .unwrap()
        .to_owned()
}

fn export(fixture: &Fixture, session: &str, name: &str) -> serde_json::Value {
    let path = fixture.root.join(name);
    let result = output(
        fixture,
        &[
            "session",
            "export",
            session,
            "--out",
            path.to_str().unwrap(),
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

pub(super) fn image_id(fixture: &Fixture, image: &Path) -> String {
    let result = image_command(fixture, image)
        .arg("version")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let banner = String::from_utf8(result.stdout).unwrap();
    let id = banner
        .split_once("; binary ")
        .and_then(|(_, rest)| rest.split([';', ')']).next())
        .unwrap();
    assert_eq!(id.len(), 12, "{banner}");
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()), "{banner}");
    id.to_owned()
}

pub(super) fn identified_image(fixture: &Fixture, source: &Path, tag: char) -> PathBuf {
    let directory = fixture
        .root
        .join(".local/lib/plak/maintainer")
        .join(tag.to_string().repeat(64));
    std::fs::create_dir_all(&directory).unwrap();
    let image = directory.join("plakd");
    std::fs::copy(source, &image).unwrap();
    let output = image_command(fixture, &image)
        .args(["version", "--write-binary-manifest"])
        .arg(&directory)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    image
}

/// Starts `image` as this fixture's host, with `env` set.
fn start_image(fixture: &Fixture, image: &Path, env: &[(&str, String)]) -> Host {
    serving_host(spawn_host(
        image_command(fixture, image)
            .arg("host")
            .envs(env.iter().map(|(name, value)| (*name, value.as_str()))),
    ))
    .expect("the successor host started")
}

/// The restart upgrade: a planned stop of `host`, then `image` on the same
/// stores. Returns the successor.
fn restart_to(fixture: &Fixture, host: &mut Host, image: &Path, env: &[(&str, String)]) -> Host {
    stop(host);
    start_image(fixture, image, env)
}

/// The held guard in this disposable home proves the process belongs to this fixture.
pub(super) struct HomeHost(pub(super) PathBuf);

impl Drop for HomeHost {
    fn drop(&mut self) {
        let path = self.0.join("host/host.lock");
        let Ok(file) = std::fs::File::open(&path) else {
            return;
        };
        if !matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
            return;
        }
        let Ok(holder) = std::fs::read_to_string(path) else {
            return;
        };
        let Some(pid) = holder
            .split_whitespace()
            .next()
            .and_then(|pid| pid.parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw)
        else {
            return;
        };
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        let deadline = Instant::now() + WAIT;
        while matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub(super) fn dispatches(fixture: &Fixture) -> usize {
    use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
    read_evidence(fixture.root.join("evidence.jsonl"))
        .unwrap()
        .iter()
        .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
        .count()
}

/// Starts a host that runs one daemon-owned job on the paired placement.
pub(super) fn placed_job(
    fixture: &Fixture,
    path: &str,
    id: &str,
) -> (Host, Client, String, NodeAddress) {
    let host = start_env(fixture, &[], &[("PATH", path.to_owned())]);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, id);
    let mut request = new_session(&fixture.workspace());
    if let Op::NewSession { runner, .. } = &mut request {
        *runner = plakous_host_protocol::wire::RunnerLabel {
            name: "remote".into(),
            runner_id: id.into(),
        };
    }
    let Outcome::Ok(Answer::Created { session, node, .. }) = client.request(1, request) else {
        panic!("remote creation refused")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "start a job on the paired placement", ANSWER);
    runner::job(fixture);
    (host, client, session, node)
}

fn identity(fixture: &Fixture, id: &str) -> (plakous::sessions::SessionLocator, String) {
    let state = fixture
        .home()
        .join("state")
        .join(plakd::home::state::state_key());
    let product = rusqlite::Connection::open_with_flags(
        state.join("product.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let locator: String = product
        .query_row("SELECT locator FROM sessions WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .unwrap();
    let locator: plakous::sessions::SessionLocator = locator.parse().unwrap();
    let native = rusqlite::Connection::open_with_flags(
        state.join("runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let line = native
        .query_row(
            "SELECT line_id FROM branches WHERE session_id=?1 AND branch_id=?2",
            rusqlite::params![
                locator.session_id().to_string(),
                locator.branch_id().to_string()
            ],
            |row| row.get(0),
        )
        .unwrap();
    (locator, line)
}
