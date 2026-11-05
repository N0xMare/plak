//! Spec 03 §1.2: a session's genesis roster is read from its own workspace.
//! A trusted project's template in its `[roster] working_set` is frozen into
//! the session's genesis profile, so its root may spawn it; an untrusted
//! project's is not.

use super::*;
use plak::home::paths::PlakPaths;

/// The working set the session's genesis froze, read from the product
/// store's root profile.
fn frozen_working_set(fixture: &Fixture) -> Vec<String> {
    let database = PlakPaths::explicit(fixture.home()).product_database();
    let connection =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let profile: String = connection
        .query_row(
            "SELECT root_profile FROM sessions WHERE root_profile IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let profile: Value = serde_json::from_str(&profile).unwrap();
    profile["working_set"]
        .as_array()
        .unwrap_or_else(|| panic!("a working set: {profile}"))
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect()
}

/// Runs one prompt in a workspace whose project names `projkid` in its
/// working set, trusted or not, and returns the working set its genesis
/// froze.
fn genesis_working_set(trusted: bool) -> Vec<String> {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "done"}}]
    }));
    let project = fixture.workspace().join(".plak");
    std::fs::create_dir_all(project.join("agents")).unwrap();
    std::fs::write(
        project.join("config.toml"),
        "[roster]\nworking_set = [\"projkid\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.join("agents/projkid.toml"),
        "account = \"codex\"\nmodel = \"offline-loopback-v1\"\neffort = \"medium\"\ndescription = \"a project child\"\n",
    )
    .unwrap();
    if trusted {
        let trust = run_cli(&fixture, &["config", "trust"]);
        assert!(trust.status.success(), "{}", trust.stderr);
    }
    let run = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--",
            "hello",
        ],
    );
    assert!(run.status.success(), "{}", run.stderr);
    frozen_working_set(&fixture)
}

#[test]
fn a_trusted_projects_template_is_frozen_into_the_genesis_and_an_untrusted_ones_is_not() {
    assert_eq!(genesis_working_set(true), ["projkid"]);
    assert!(
        !genesis_working_set(false).contains(&"projkid".to_owned()),
        "an untrusted project's template reached the genesis"
    );
}
