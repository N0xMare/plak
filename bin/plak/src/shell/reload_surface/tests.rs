use super::*;
use crate::surface_protocol::HostEvent;

const SESSION: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

fn home() -> (tempfile::TempDir, PlakPaths) {
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout().unwrap();
    std::fs::write(paths.config_file(), "[tui]\nreload_requests = true\n").unwrap();
    (root, paths)
}

fn outcome(paths: &PlakPaths, request_id: &str) -> Option<serde_json::Value> {
    let bytes = std::fs::read(crate::reload_request::outcome_path(paths, request_id)).ok()?;
    Some(serde_json::from_slice(&bytes).unwrap())
}

/// What one check answered: the build status, and the request offered.
fn checked(channel: &ReloadChannel, session: Option<&str>) -> Option<ReloadRequest> {
    let (sender, received) = plakous_host_protocol::observation_channel();
    channel.check(session, &Sender::over(sender));
    assert!(matches!(
        received.try_recv().unwrap(),
        HostEvent::Extension(SurfaceObservation::BuildStatus { .. })
    ));
    match received.try_recv() {
        Ok(HostEvent::Extension(SurfaceObservation::ReloadRequested(request))) => Some(request),
        Ok(other) => panic!("{other:?}"),
        Err(_) => None,
    }
}

/// The host announces the session it serves and offers its waiting
/// request on every check until the face accepts it, so a lost answer loses
/// nothing; it records a refusal only for a request it handed over.
#[test]
fn a_request_is_offered_until_accepted_and_refused_only_once_handed_over() {
    let (_root, paths) = home();
    let channel = ReloadChannel::new(paths.clone());
    assert!(checked(&channel, None).is_none());
    assert!(
        crate::reload_request::request(&paths, SESSION, "build-1", "build-1").is_err(),
        "no session was announced yet"
    );
    assert!(checked(&channel, Some(SESSION)).is_none(), "nothing waits");
    crate::reload_request::request(&paths, SESSION, "build-1", "build-1").unwrap();
    let offered = checked(&channel, Some(SESSION)).expect("the waiting request is offered");
    assert_eq!(offered.expect_binary, "build-1");
    assert_eq!(
        checked(&channel, Some(SESSION)),
        Some(offered.clone()),
        "an unaccepted request is offered again"
    );

    channel.refuse(&offered.request_id, "not yet handed over".to_owned());
    assert!(
        outcome(&paths, &offered.request_id).is_none(),
        "only a request the face accepted can be refused"
    );
    channel.accept(&offered.request_id);
    assert!(checked(&channel, Some(SESSION)).is_none(), "accepted once");
    channel.refuse(&offered.request_id, "sub-agents are live".to_owned());
    let refused = outcome(&paths, &offered.request_id).unwrap();
    assert_eq!(refused["outcome"], "refused");
    assert_eq!(refused["reason"], "sub-agents are live");
}

/// A request offered and never accepted is refused for its requester when
/// the host ends, so it does not wait out its timeout.
#[test]
fn an_offered_request_no_face_took_is_refused_when_the_host_ends() {
    let (_root, paths) = home();
    let channel = ReloadChannel::new(paths.clone());
    checked(&channel, Some(SESSION));
    crate::reload_request::request(&paths, SESSION, "build-1", "build-1").unwrap();
    let offered = checked(&channel, Some(SESSION)).unwrap();
    drop(channel);
    let refused = outcome(&paths, &offered.request_id).unwrap();
    assert_eq!(refused["outcome"], "refused");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap()
            .starts_with("not taken:")
    );
}

/// Only a session's UUID names a file in the channel: a session that is not
/// one is never announced, and nothing outside the channel is touched.
#[test]
fn only_a_uuid_session_is_announced() {
    let (_root, paths) = home();
    let channel = ReloadChannel::new(paths.clone());
    std::fs::create_dir_all(paths.run_dir()).unwrap();
    let victim = paths.run_dir().join("victim.request");
    std::fs::write(&victim, b"kept").unwrap();
    assert!(checked(&channel, Some("../victim")).is_none());
    assert!(victim.exists(), "a path-like session touches no file");
    assert!(!paths.run_dir().join("victim.listener").exists());
}
