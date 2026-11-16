use super::*;

fn row(name: &str) -> Row {
    Row {
        session: name.into(),
        branch: "main".into(),
        checkpoint: "head".into(),
        archived: false,
        superseded: false,
        binding_change: None,
        unavailable: None,
        unaffected: Vec::new(),
    }
}

#[test]
fn differential_binding_preflight_refuses_only_new_live_regressions() {
    let source = Report {
        compatible: false,
        sessions: vec![
            row("changed"),
            row("unavailable"),
            Row {
                binding_change: Some("old refusal".into()),
                ..row("already-refused")
            },
        ],
    };
    let destination = Report {
        compatible: false,
        sessions: vec![
            Row {
                binding_change: Some("tool binding".into()),
                ..row("changed")
            },
            Row {
                unavailable: Some("missing executor".into()),
                ..row("unavailable")
            },
            Row {
                binding_change: Some("old refusal".into()),
                ..row("already-refused")
            },
            Row {
                archived: true,
                binding_change: Some("old refusal".into()),
                ..row("archived")
            },
            Row {
                superseded: true,
                binding_change: Some("old refusal".into()),
                ..row("superseded")
            },
            Row {
                unaffected: vec!["MCP external contract unaffected".into()],
                ..row("external")
            },
        ],
    };
    assert_eq!(
        regressions(Some(&source), &destination)
            .iter()
            .map(|r| r.session.as_str())
            .collect::<Vec<_>>(),
        ["changed", "unavailable"]
    );
    assert_eq!(
        regressions(None, &destination)
            .iter()
            .map(|r| r.session.as_str())
            .collect::<Vec<_>>(),
        ["changed", "already-refused"]
    );
}

#[test]
fn live_binding_preflight_covers_every_selected_active_branch_and_no_unrelated_root() {
    let sessions = BTreeSet::from(["root".into(), "child".into()]);
    let report = |rows| Report {
        compatible: true,
        sessions: rows,
    };
    let source = || report(vec![row("root"), row("child")]);
    assert!(qualify_selected(source(), report(vec![row("root")]), &sessions).is_err());
    assert!(qualify_selected(report(vec![row("root")]), source(), &sessions).is_err());
    let changed = Row {
        binding_change: Some("tool binding".into()),
        ..row("child")
    };
    assert!(qualify_selected(source(), report(vec![row("root"), changed]), &sessions).is_err());
    let mut child = row("child");
    child.checkpoint = "a-later-head-while-the-source-runs".into();
    let unrelated = Row {
        binding_change: Some("unrelated".into()),
        ..row("unselected")
    };
    assert!(
        qualify_selected(
            source(),
            report(vec![row("root"), child, unrelated]),
            &sessions
        )
        .is_ok()
    );
    let other_branch = Row {
        branch: "other".into(),
        ..row("child")
    };
    assert!(
        qualify_selected(
            report(vec![row("root"), row("child"), other_branch]),
            source(),
            &sessions
        )
        .is_err()
    );
}

#[test]
fn live_binding_preflight_does_not_hide_a_known_cut_behind_external_unavailability() {
    let sessions = BTreeSet::from(["root".into()]);
    let report = |binding_change| Report {
        compatible: false,
        sessions: vec![Row {
            binding_change,
            unavailable: Some("external executor availability is unknown".into()),
            ..row("root")
        }],
    };
    assert!(qualify_selected(report(None), report(None), &sessions).is_ok());
    assert!(
        qualify_selected(
            report(None),
            report(Some("tool binding changed".into())),
            &sessions
        )
        .is_err()
    );
}
