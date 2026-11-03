use super::*;

#[test]
fn a_node_admits_only_portable_ids() {
    let node = SessionNode::genesis("0b8c1c55-6a35-4c58-9d0e-d1d7e1f4a001");
    assert_eq!(node.agent_id, node.tree_id);
    assert_eq!(
        SessionNode::parse(&node.tree_id, "c39"),
        Ok(SessionNode {
            tree_id: node.tree_id.clone(),
            agent_id: "c39".to_owned(),
        })
    );
    for (tree, agent) in [("", "root"), ("t", ""), ("t:1", "root"), ("t", "ro ot")] {
        assert!(
            SessionNode::parse(tree, agent).is_err(),
            "{tree:?}/{agent:?}"
        );
    }
    assert!(SessionNode::parse(&"t".repeat(129), "root").is_err());
}

#[test]
fn a_root_profile_round_trips_and_refuses_what_this_build_did_not_write() {
    let profile = RootProfile {
        root_template: Some("lead".to_owned()),
        working_set: vec!["worker".to_owned(), "reviewer".to_owned()],
    };
    let encoded = profile.encode().unwrap();
    assert_eq!(
        encoded,
        r#"{"root_template":"lead","working_set":["worker","reviewer"]}"#
    );
    assert_eq!(RootProfile::decode(&encoded), Ok(profile));
    for corrupt in [
        r#"{"root_template":null}"#,
        r#"{"root_template":null,"working_set":[],"extra":1}"#,
        r#"{"root_template":"Lead","working_set":[]}"#,
        r#"{"root_template":null,"working_set":["worker","worker"]}"#,
        "not json",
    ] {
        assert!(RootProfile::decode(corrupt).is_err(), "{corrupt}");
    }
}

#[test]
fn a_runner_display_round_trips_and_refuses_malformed_copies() {
    let display = RunnerDisplay {
        name: "buildbox".to_owned(),
        runner_id: "runner-1".to_owned(),
        workspace_root: PathBuf::from("/w"),
        workspace_cwd: PathBuf::from("/w/crate"),
    };
    assert_eq!(
        RunnerDisplay::decode(&display.encode().unwrap()),
        Ok(display.clone())
    );
    let outside = RunnerDisplay {
        workspace_cwd: PathBuf::from("/elsewhere"),
        ..display.clone()
    };
    assert!(outside.encode().is_err());
    let relative = RunnerDisplay {
        workspace_root: PathBuf::from("w"),
        workspace_cwd: PathBuf::from("w"),
        ..display.clone()
    };
    assert!(relative.encode().is_err());
    let blank = RunnerDisplay {
        runner_id: "runner 1".to_owned(),
        ..display
    };
    assert!(blank.encode().is_err());
    assert!(
        RunnerDisplay::decode(r#"{"name":"buildbox","runner_id":"r","workspace_root":"/w"}"#)
            .is_err()
    );
}
