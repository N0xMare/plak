use super::*;

fn row(name: &str) -> Row {
    Row {
        session: name.into(),
        branch: "main".into(),
        checkpoint: "head".into(),
        archived: false,
        superseded: false,
        child: false,
        closed: false,
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
                closed: true,
                child: true,
                binding_change: Some("retired child refusal".into()),
                ..row("closed")
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
fn a_child_binding_regression_reports_the_count_and_change_without_exact_reopen() {
    let report = Report {
        compatible: false,
        sessions: vec![Row {
            child: true,
            binding_change: Some("provider configuration".into()),
            ..row("child")
        }],
    };
    let failure = finish(None, &report).unwrap_err();
    assert!(failure.message().contains(
        "host_upgrade_child_reopen_refused: 1 affected child session(s); binding change: provider configuration"
    ));
}
