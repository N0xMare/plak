use super::*;

#[test]
fn changed_native_protocol_refuses_without_stopping_the_job() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let mut environment = candidate_environment(&image).to_vec();
    environment.push((
        "PLAK_TEST_HOST_UPGRADE_RUNNER_PROTOCOL",
        (plakous::runner::protocol::PROTOCOL_VERSION + 1).to_string(),
    ));
    source_script(&fixture);
    let mut host = start_env(&fixture, &[], &environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    let root = start_root(&fixture, &mut client, "local", None, 1);
    // Qualification can outlast the host's write deadline for an attached
    // client that has stopped reading observations.
    drop(client);
    let result = candidate_upgrade(&fixture, &image, &environment)
        .output()
        .unwrap();
    assert!(!result.status.success(), "{result:?}");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("host_upgrade_runner_protocol"),
        "{result:?}"
    );
    assert!(host.child.as_mut().unwrap().try_wait().unwrap().is_none());
    assert_current_jobs(&fixture, &host, &root);
    finish_source(&fixture, &mut host, &root);
}

#[test]
fn third_build_stops_the_job_on_its_original_daemon() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let first = replacement_image(&fixture).canonicalize().unwrap();
    let second = replacement_image_named(&fixture, "subsequent")
        .canonicalize()
        .unwrap();
    let paired = Paired::new(&fixture);
    let mut environment = candidate_environment(&first).to_vec();
    environment.extend([
        (
            "PLAK_TEST_HOST_UPGRADE_RUNNER_NEXT_IMAGE",
            second.display().to_string(),
        ),
        (
            "PLAK_TEST_HOST_UPGRADE_RUNNER_NEXT_REVISION",
            NEXT_REVISION.into(),
        ),
        ("PATH", paired.path.clone()),
    ]);
    let mut host = start_env(&fixture, &[], &environment);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &paired.id);
    let mut request = new_session(&fixture.workspace());
    let Op::NewSession { runner, .. } = &mut request else {
        unreachable!()
    };
    *runner = plakous_host_protocol::wire::RunnerLabel {
        name: "remote".into(),
        runner_id: paired.id.clone(),
    };
    let Outcome::Ok(Answer::Created { session, node, .. }) = client.request(1, request) else {
        panic!("paired creation refused")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "start a carried job", ANSWER);
    let (pid, daemon) = runner::job(&fixture);
    profiles::wait_idle(&mut client, &session);
    settled_script(&fixture);
    for image in [&first, &second] {
        let bridges: Vec<i32> = std::fs::read_to_string(fixture.root.join("ssh-bridges"))
            .unwrap()
            .lines()
            .map(|pid| pid.parse().unwrap())
            .filter(|&pid| runner::alive(pid))
            .collect();
        assert!(!bridges.is_empty(), "no active source bridge");
        paired.advance(image);
        let result = candidate_upgrade(&fixture, image, &environment)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        for pid in bridges {
            assert!(!runner::alive(pid), "source bridge {pid} survived exec");
        }
        drop(client);
        client = Client::connect(&host);
        runner::remote_ready(&mut client, &paired.id);
        client.attach(20, &node);
        assert!(runner::alive(pid));
        assert_eq!(runner::job(&fixture), (pid, daemon));
        running_job(&fixture, &session);
    }
    stop_job(&mut client, session, pid);
    stop(&mut host);
    terminal_rows(&fixture, 1);
}

fn assert_current_jobs(fixture: &Fixture, host: &Host, root: &Root) {
    let mut client = Client::connect(host);
    assert_eq!(client.request(9, Op::Ping), Outcome::Ok(Answer::Pong));
    assert_eq!(marked(fixture, &root.name, "fg"), root.foreground);
    assert_eq!(marked(fixture, &root.name, "bg"), root.background);
    assert!(runner::alive(root.background.0) && runner::alive(root.foreground.0));
}

fn finish_source(fixture: &Fixture, host: &mut Host, root: &Root) {
    for job in ["bg", "fg"] {
        std::fs::write(fixture.root.join(format!("{}-{job}-finish", root.name)), "").unwrap();
    }
    wait_exit(root.background.0);
    wait_exit(root.foreground.0);
    stop(host);
}
