//! An in-turn fold belongs to the held driver, not a separate manual-fold owner.
use super::*;

#[test]
fn host_step_drain_in_turn_paid_fold_at_bound_rolls_back_without_a_second_summary() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let mut script = json!({
        "version":1, "maximum_projected_input":20_000,
        "scheduled_compaction":true, "content_sensitive_projection":true,
        "tool_result_bytes":7_500, "fold":{"stall":75_000},
        "steps":[{"output":{"text":"retained answer"},"finish":"stop"}]
    });
    let session = folding_seed_with(&fixture, &script);
    script["steps"] = json!([
        {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls","usage":{}},
        {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls","usage":{}},
        {"output":{"text":ANSWER},"finish":"stop","usage":{}}
    ]);
    fixture.write_script(&script);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let node = client
        .rows(1)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap()
        .node;
    let attachment = client.attach(2, &node);
    prompt(
        &mut client,
        attachment,
        "continue the protected turn through a scheduled fold",
    );
    // This test observes the fold out of band. Keep the control connection,
    // but do not leave unread transcript output filling its socket meanwhile.
    assert_eq!(
        client.request(9, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    let summaries = fixture.root.join("evidence.summary.jsonl");
    let deadline = Instant::now() + WAIT;
    while !read_evidence(&summaries).is_ok_and(|rows| !rows.is_empty()) {
        assert!(
            Instant::now() < deadline,
            "scheduled paid fold never dispatched"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let refused = output_env(
        &fixture,
        &[
            "host",
            "upgrade",
            "--expect-binary",
            &binary(&fixture),
            "--drain",
            "0",
        ],
        false,
        &[("PLAK_TEST_FORCE_HOST_UPGRADE", "1")],
    );
    assert!(!refused.status.success());
    let detail = String::from_utf8_lossy(&refused.stderr);
    assert!(
        detail.contains("host_step_drain_blocked") && detail.contains("paid_compaction_in_flight"),
        "{detail}"
    );
    assert_eq!(client.request(10, Op::Ping), Outcome::Ok(Answer::Pong));
    assert!(matches!(
        client.request(11, fresh_session(&fixture)),
        Outcome::Ok(Answer::Created { .. })
    ));
    assert!(!fixture.home().join("host/upgrades").exists());
    assert_eq!(root_record(&fixture, &session).1, 0);
    settled(&fixture, &session);
    assert_eq!(
        read_evidence(&summaries).unwrap().len(),
        1,
        "rollback must neither cancel nor redispatch the paid fold"
    );
    assert!(
        store(&fixture)
            .load_views(&session)
            .unwrap()
            .unwrap()
            .state
            .open_turns
            .is_empty()
    );
    stop(&mut host);
}
