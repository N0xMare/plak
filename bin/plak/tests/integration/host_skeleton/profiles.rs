//! Credential-free profile transitions through the real host socket.

use super::*;
use plak::store::session_records::SessionRecordStore as _;
use plakd_wire_types::{ProfileApply, ProfileOutcome, SurfaceIntent, SurfaceObservation};
use plakous_host_protocol::SessionObservation;

const TARGET_ANSWER: &str = "the target profile answer";
const REOPENED_ANSWER: &str = "the reopened profile answer";

#[derive(Debug)]
enum Application {
    Applied(ProfileOutcome),
    Moved(NodeAddress),
}

fn profile_fixture() -> Option<Fixture> {
    let fixture = fixture()?;
    fixture.write_script(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [
            {"output": {"text": ANSWER}},
            {"output": {"text": TARGET_ANSWER}}
        ]
    }));
    let profiles = fixture.home().join("profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    write_private(
        &fixture.home().join("config.toml"),
        "[root]\nagent = \"lead\"\n[orchestration]\nsubagents = true\n[compaction]\nenabled = true\n",
    );
    for (name, model, effort) in [
        ("offline-low", "offline-loopback-v1", "low"),
        ("offline-medium", "offline-loopback-v1", "medium"),
        ("unready", "gpt-5.3-codex", "medium"),
        ("bad-effort", "offline-loopback-v1", "high"),
    ] {
        write_private(
            &fixture.home().join(format!("agents/{name}.toml")),
            &format!(
                "account = \"codex\"\nmodel = \"{model}\"\neffort = \"{effort}\"\ncomposition = \"direct\"\ndescription = \"profile fixture\"\n"
            ),
        );
        write_private(
            &profiles.join(format!("{name}.toml")),
            &format!("root = \"{name}\"\nworking_set = [\"lead\"]\n"),
        );
    }
    let low = std::fs::read_to_string(fixture.home().join("agents/offline-low.toml")).unwrap();
    write_private(
        &fixture.home().join("agents/offline-fast.toml"),
        &format!("{low}fast_mode = true\n"),
    );
    write_private(
        &profiles.join("offline-fast.toml"),
        "root = \"offline-fast\"\nworking_set = [\"lead\"]\n",
    );
    Some(fixture)
}

fn apply(
    client: &mut Client,
    attachment: u64,
    name: &str,
    apply: ProfileApply,
) -> Result<Application, String> {
    assert_eq!(
        client.request(
            10,
            Op::Intent {
                client_id: None,
                attachment,
                intent: SessionIntent::Extension(SurfaceIntent::ApplyProfile {
                    name: name.to_owned(),
                    apply,
                }),
                blobs: Vec::new(),
            },
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    receive_application(client)
}

fn receive_application(client: &mut Client) -> Result<Application, String> {
    loop {
        match client.next() {
            // A successful branch ends the source attachment before the
            // profile verdict. Inspect the successor after attaching below.
            HostFrame::Moved { node, .. } => return Ok(Application::Moved(node)),
            HostFrame::Event {
                observation: SessionObservation::Extension(observation),
                ..
            } => match observation {
                SurfaceObservation::ProfileApplied { outcome, .. } => {
                    return Ok(Application::Applied(outcome));
                }
                SurfaceObservation::ProfileRefused { reason, .. } => return Err(reason),
                _ => {}
            },
            _ => {}
        }
    }
}

pub(super) fn wait_idle(client: &mut Client, session: &str) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let rows = client.rows(20);
        if rows
            .iter()
            .any(|row| row.session == session && row.activity == SessionActivity::Idle)
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the turn stayed busy: {rows:?}"
        );
        std::thread::yield_now();
    }
}

fn attach_history(host: &Host, node: &NodeAddress, answer: &str) -> String {
    let mut client = Client::connect(host);
    let attachment = client.attach(1, node);
    let mut snapshot = plakous_host_protocol::wire::SnapshotAssembler::default();
    let mut transcript = plakous_host_protocol::views::transcript::TranscriptState::default();
    loop {
        match client.next() {
            HostFrame::Snapshot(page) if page.attachment == attachment => {
                if let Some(snapshot) = snapshot.push(page).unwrap() {
                    transcript.replace(snapshot).unwrap();
                }
            }
            HostFrame::Event {
                attachment: held,
                observation: SessionObservation::TranscriptPatch(patch),
                ..
            } if held == attachment => transcript.apply(patch).unwrap(),
            _ => continue,
        }
        let history = format!("{transcript:?}");
        if history.contains(answer) {
            return history;
        }
    }
}

fn source_preserved(host: &Host, client: &mut Client, source: &str, node: &NodeAddress) {
    let rows = client.rows(21);
    let row = rows.iter().find(|row| row.session == source).unwrap();
    assert_eq!(&row.node, node, "the source address changed after a fork");
    let history = attach_history(host, &row.node, ANSWER);
    assert!(history.contains("one admitted turn"), "{history}");
    assert!(history.contains(ANSWER), "{history}");
    assert!(!history.contains(TARGET_ANSWER), "{history}");
    assert!(
        !history.contains("after the profile transition"),
        "{history}"
    );
}

fn refresh_readiness(client: &mut Client, attachment: u64) {
    assert_eq!(
        client.request(
            5,
            Op::Intent {
                client_id: None,
                attachment,
                intent: SessionIntent::RefreshReadiness,
                blobs: Vec::new(),
            },
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    loop {
        if matches!(
            client.next(),
            HostFrame::Event {
                observation: SessionObservation::ProviderSnapshot { .. },
                ..
            }
        ) {
            break;
        }
    }
}

fn transition(mode: ProfileApply, expected: ProfileOutcome) {
    let Some(fixture) = profile_fixture() else {
        return;
    };
    let config = std::fs::read(fixture.home().join("config.toml")).unwrap();
    let mut host = start(&fixture);
    let (mut client, source, node, attachment) = requests::seeded(&fixture, &host);
    client.retain_observations();
    refresh_readiness(&mut client, attachment);
    let moved = match apply(&mut client, attachment, "offline-low", mode).unwrap() {
        Application::Applied(outcome) => {
            assert_eq!(outcome, expected);
            None
        }
        Application::Moved(node) => Some(node),
    };
    assert_eq!(
        crash::dispatches(&fixture),
        1,
        "a profile transition dispatched inference"
    );
    let rows = client.rows(11);
    let target = rows.iter().find(|row| row.session != source).unwrap();
    let target_node = target.node.clone();
    let target_session = target.session.clone();
    if let Some(moved) = &moved {
        assert_eq!(&target_node, moved, "directory and moved disagree");
    }
    // A switch keeps the line's node; a fork starts another line.
    assert_eq!(target_node == node, mode == ProfileApply::Switch);
    let attachment = client.attach(12, &target_node);
    if moved.is_some() {
        let Application::Applied(outcome) = receive_application(&mut client).unwrap() else {
            panic!("the successor did not acknowledge its profile");
        };
        assert_eq!(outcome, expected);
    }
    client.prompt(
        13,
        attachment,
        "after the profile transition",
        TARGET_ANSWER,
    );
    wait_idle(&mut client, &target_session);
    if mode == ProfileApply::Fork {
        source_preserved(&host, &mut client, &source, &node);
        let history = attach_history(&host, &target_node, TARGET_ANSWER);
        assert!(history.contains(ANSWER), "{history}");
        assert!(history.contains(TARGET_ANSWER), "{history}");
    }
    drop(client);
    stop(&mut host);
    assert_eq!(
        std::fs::read(fixture.home().join("config.toml")).unwrap(),
        config
    );

    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let store = plak::store::session_records::SqliteSessionStore::new(paths);
    let target = store.load(&target_session).unwrap();
    assert_eq!(target.model.as_deref(), Some("offline-loopback-v1"));
    assert_eq!(target.effort.as_deref(), Some("low"));
    assert_eq!(
        target.root_profile.unwrap().root_template.as_deref(),
        Some("offline-low")
    );
    requests::history_preserved(&fixture, &target_session);

    // Exact reopen must bind the published successor, not resurrect its source.
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": REOPENED_ANSWER}}]
    }));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.retain_observations();
    if mode == ProfileApply::Fork {
        source_preserved(&host, &mut client, &source, &node);
    }
    let attachment = client.attach(14, &target_node);
    client.prompt(15, attachment, "after exact reopen", REOPENED_ANSWER);
    wait_idle(&mut client, &target_session);
    drop(client);
    stop(&mut host);
    assert_eq!(crash::dispatches(&fixture), 3);
}

#[test]
fn offline_profile_switch_runs_and_reopens_through_the_host_socket() {
    transition(ProfileApply::Switch, ProfileOutcome::Switched);
}

#[test]
fn offline_profile_fork_runs_and_reopens_through_the_host_socket() {
    transition(ProfileApply::Fork, ProfileOutcome::Forked);
}

#[test]
fn offline_profile_readiness_does_not_admit_other_routes_or_efforts() {
    let Some(fixture) = profile_fixture() else {
        return;
    };
    let mut host = start(&fixture);
    let (mut client, source, _, attachment) = requests::seeded(&fixture, &host);
    client.retain_observations();
    let defaults = std::fs::read(fixture.home().join("config.toml")).unwrap();
    for mode in [ProfileApply::Switch, ProfileApply::Fork] {
        let reason = apply(&mut client, attachment, "unready", mode).unwrap_err();
        assert!(reason.contains("codex is not ready"), "{reason}");
    }
    assert_eq!(
        client.request(
            10,
            Op::Intent {
                client_id: None,
                attachment,
                intent: SessionIntent::Extension(SurfaceIntent::ApplyProfile {
                    name: "unready".into(),
                    apply: ProfileApply::Default,
                }),
                blobs: Vec::new(),
            }
        ),
        Outcome::Refused(
            plakous_host_protocol::wire::RequestRefusal::RefusedForRole {
                row: "local_face".into()
            }
        )
    );
    assert_eq!(
        std::fs::read(fixture.home().join("config.toml")).unwrap(),
        defaults,
        "remote default refusal cannot rewrite the home"
    );
    let reason = apply(&mut client, attachment, "bad-effort", ProfileApply::Fork).unwrap_err();
    assert!(reason.contains("effort"), "{reason}");
    let reason = apply(
        &mut client,
        attachment,
        "offline-fast",
        ProfileApply::Switch,
    )
    .unwrap_err();
    assert!(reason.contains("Fast is absent"), "{reason}");
    let reason = apply(
        &mut client,
        attachment,
        "offline-medium",
        ProfileApply::Switch,
    )
    .unwrap_err();
    assert!(reason.contains("does not change"), "{reason}");
    let rows = client.rows(11);
    assert_eq!(rows.len(), 1, "a refusal created a session: {rows:?}");
    assert_eq!(rows[0].session, source);
    assert_eq!(
        crash::dispatches(&fixture),
        1,
        "a refusal dispatched inference"
    );
    client.prompt(
        12,
        attachment,
        "source still works after refusal",
        TARGET_ANSWER,
    );
    wait_idle(&mut client, &source);
    drop(client);
    stop(&mut host);
}
