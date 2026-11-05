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

/// A probe for no session is refused as such, not as a failed check.
#[test]
fn a_probe_with_no_session_is_refused_as_such() {
    let (_root, paths) = home();
    let (sender, received) = plakous_host_protocol::observation_channel();
    probe(&paths, None, &Sender::over(sender));
    assert!(matches!(
        received.try_recv().unwrap(),
        HostEvent::Extension(SurfaceObservation::ReloadProbed { target: None, refusal: Some(refusal) })
            if refusal == NO_SESSION
    ));
}

/// A probe that never answers (it could not run) is answered as a failed
/// check.
#[test]
fn an_unanswered_probe_is_answered_as_failed() {
    let (sender, received) = plakous_host_protocol::observation_channel();
    drop(ProbeAnswer(Some(Sender::over(sender))));
    assert!(matches!(
        received.try_recv().unwrap(),
        HostEvent::Extension(SurfaceObservation::ReloadProbeFailed)
    ));
}

/// A reload request belongs to its session: when the host leaves it, every
/// request it holds for it (accepted, offered or still waiting) is settled
/// `not taken`, its announcement goes, and none is offered for the next
/// session. The face's late refusal is not recorded over it.
#[test]
fn leaving_a_session_settles_its_requests_as_not_taken() {
    const NEXT: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let (_root, paths) = home();
    let channel = ReloadChannel::new(paths.clone());
    checked(&channel, Some(SESSION));
    crate::reload_request::request(&paths, SESSION, "build-1", "build-1").unwrap();
    let accepted = checked(&channel, Some(SESSION)).unwrap();
    channel.accept(&accepted.request_id);
    crate::reload_request::request(&paths, SESSION, "build-1", "build-1").unwrap();
    let offered = checked(&channel, Some(SESSION)).unwrap();
    let (_, waiting) =
        crate::reload_request::request(&paths, SESSION, "build-1", "build-1").unwrap();

    channel.leave(NEXT);
    assert!(
        outcome(&paths, &accepted.request_id).is_none(),
        "leaving another session settles nothing"
    );
    channel.leave(SESSION);
    let waiting: serde_json::Value =
        serde_json::from_slice(&std::fs::read(waiting).unwrap()).unwrap();
    for settled in [
        outcome(&paths, &accepted.request_id).unwrap(),
        outcome(&paths, &offered.request_id).unwrap(),
        waiting,
    ] {
        assert_eq!(settled["outcome"], "refused");
        assert_eq!(settled["reason"], "not taken: the session switched");
    }
    channel.refuse(&accepted.request_id, "a late refusal".to_owned());
    assert_eq!(
        outcome(&paths, &accepted.request_id).unwrap()["reason"],
        "not taken: the session switched"
    );
    assert!(
        crate::reload_request::request(&paths, SESSION, "build-1", "build-1").is_err(),
        "the session the host left is no longer announced"
    );
    assert!(
        checked(&channel, Some(NEXT)).is_none(),
        "nothing carries over"
    );
}

/// The real host settles a request the face accepted, and one it was
/// offered, before an in-host `/new` or `/resume` completes, and never
/// offers them to the session it switched to.
#[test]
fn a_session_switch_settles_its_reload_requests_before_completing() {
    use crate::surface_protocol::{SurfaceIntent, UserAction};
    for resume in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _, paths) =
            crate::composition_tests::product_injected_attached_host(root.path());
        std::fs::write(paths.config_file(), "[tui]\nreload_requests = true\n").unwrap();
        let switch = if resume {
            let (mut target, target_runtime, _) =
                crate::composition_tests::product_injected_attached_host(root.path());
            crate::store::sessions::save_session(&paths, &mut target.session).unwrap();
            let id = target.session.id.clone();
            plakous_hosted::runtime::run_scripted_host(
                target,
                target_runtime,
                vec![UserAction::Quit],
            )
            .unwrap();
            UserAction::ResumeSession { id: Some(id) }
        } else {
            UserAction::NewSession
        };
        crate::store::sessions::save_session(&paths, &mut config.session).unwrap();
        let source = config.session.id.clone();
        let (actions, events, worker) = plakous_hosted::runtime::spawn_host_worker(config).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        // The first event `hit` matches; an offer before it fails `strict`.
        let next = |what: &str, strict: bool, hit: &dyn Fn(&HostEvent) -> bool| loop {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap_or_else(|_| panic!("waiting for {what}"));
            assert!(
                !strict || !is_offer(&event),
                "{resume}: a request of the session left was offered: {event:?}"
            );
            if hit(&event) {
                return event;
            }
        };
        let check = || {
            actions
                .send(UserAction::Extension(SurfaceIntent::CheckBuild))
                .unwrap();
        };
        let offered_of = |event: HostEvent| match event {
            HostEvent::Extension(SurfaceObservation::ReloadRequested(request)) => request,
            other => panic!("{other:?}"),
        };
        next(
            "the attached binding",
            false,
            &|event| matches!(event, HostEvent::Binding { session_id: Some(id), .. } if *id == source),
        );
        // The host announces the session it serves on its first idle check.
        let announced = (0..50).any(|_| {
            check();
            next("the announcing check", false, &is_status);
            crate::reload_request::request(&paths, &source, "build-1", "build-1").is_ok()
        });
        assert!(announced, "{resume}: the host never announced its session");
        check();
        let accepted = offered_of(next("the first offer", false, &is_offer));
        actions
            .send(UserAction::Extension(SurfaceIntent::AcceptReloadRequest {
                request_id: accepted.request_id.clone(),
            }))
            .unwrap();
        crate::reload_request::request(&paths, &source, "build-1", "build-1").unwrap();
        check();
        let offered = offered_of(next("the second offer", false, &|event| {
            matches!(event, HostEvent::Extension(SurfaceObservation::ReloadRequested(request))
                if request.request_id != accepted.request_id)
        }));

        actions.send(switch).unwrap();
        next(
            "the switched binding",
            false,
            &|event| matches!(event, HostEvent::Binding { session_id: Some(id), .. } if *id != source),
        );
        for request in [&accepted, &offered] {
            let settled = outcome(&paths, &request.request_id)
                .unwrap_or_else(|| panic!("{resume}: no outcome for {}", request.request_id));
            assert_eq!(settled["outcome"], "refused", "{resume}");
            assert_eq!(
                settled["reason"], "not taken: the session switched",
                "{resume}"
            );
        }
        check();
        next("a check for the next session", true, &is_status);
        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();
        assert!(
            !events.try_iter().any(|event| is_offer(&event)),
            "{resume}: a request of the session left was offered again"
        );
    }
}

fn is_offer(event: &HostEvent) -> bool {
    matches!(
        event,
        HostEvent::Extension(SurfaceObservation::ReloadRequested(_))
    )
}

fn is_status(event: &HostEvent) -> bool {
    matches!(
        event,
        HostEvent::Extension(SurfaceObservation::BuildStatus { .. })
    )
}
