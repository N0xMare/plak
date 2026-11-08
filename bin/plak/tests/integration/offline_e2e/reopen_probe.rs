//! Exact reopen probes cannot borrow or mutate the live tree's ownership.

use super::*;

fn tree_version(fixture: &Fixture, session: &str) -> (i64, i64) {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let record = plak::store::ProductStore::inspect_session_read_only(&paths, session).unwrap();
    let database = rusqlite::Connection::open_with_flags(
        paths.subagents_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    database
        .query_row(
            "SELECT revision,host_generation FROM agent_trees WHERE tree_id=?1",
            [record.node.tree_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

#[test]
fn a_headless_reopen_probe_leaves_the_live_tree_unchanged_and_its_owner_keeps_saving() {
    let fixture = Fixture::new(&two_tree_turns_script());
    let config = fixture.home().join("config.toml");
    write_private(
        &config,
        &std::fs::read_to_string(&config)
            .unwrap()
            .replace("subagents = false", "subagents = true"),
    );
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
    agent.wait_for("ready", |record| record["kind"] == "ready");
    let session = hello["session_id"].as_str().unwrap();
    agent.send(&prompt_frame("run the first child"));
    agent.wait_for_completed_turns(1);
    let before = tree_version(&fixture, session);
    for _ in 0..2 {
        let probe = fixture
            .command()
            .args(["version", "--reopen-probe", session])
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let verdict: Value = serde_json::from_slice(&probe.stdout).unwrap();
        assert_eq!(verdict["verdict"], "opens", "{verdict}");
        assert_eq!(
            tree_version(&fixture, session),
            before,
            "the probe wrote the tree"
        );
    }
    agent.send(&prompt_frame("run the second child"));
    agent.wait_for_completed_turns(2);
    let after = tree_version(&fixture, session);
    assert!(
        after.0 > before.0,
        "the live owner kept saving: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.1, before.1,
        "the live owner's host generation is its own"
    );
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
}
