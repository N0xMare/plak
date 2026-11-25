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
        child_binding: None,
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
            row("closed"),
            Row {
                closed: true,
                child: true,
                ..row("previously-closed")
            },
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
                child: true,
                unavailable: Some("not comparable to the closed source".into()),
                ..row("previously-closed")
            },
            Row {
                unaffected: vec!["MCP external contract unaffected".into()],
                ..row("external")
            },
        ],
    };
    assert_eq!(
        regressions(&Baseline::from_report(Some(&source)), &destination)
            .into_iter()
            .map(|r| r.session.as_str())
            .collect::<Vec<_>>(),
        ["changed", "unavailable"]
    );
    assert_eq!(
        regressions(&Baseline::from_report(None), &destination)
            .into_iter()
            .map(|r| r.session.as_str())
            .collect::<Vec<_>>(),
        ["changed", "already-refused"]
    );
}

#[test]
fn the_binding_audit_does_not_preempt_child_reopen_qualification() {
    let diagnostic = serde_json::from_value(serde_json::json!({
        "binding_change": "provider configuration",
        "unavailable": null
    }))
    .unwrap();
    let report = Report {
        compatible: false,
        sessions: vec![Row {
            child: true,
            child_binding: Some(diagnostic),
            ..row("child")
        }],
    };
    let mut source = row("child");
    source.child = true;
    let source = Report {
        compatible: true,
        sessions: vec![source],
    };
    assert!(finish(None, &report).is_ok());
    assert!(finish(Some(&source), &report).is_ok());
}

#[test]
fn a_new_full_decode_failure_in_a_child_still_refuses() {
    let clean = Row {
        child: true,
        ..row("child")
    };
    let source = Report {
        compatible: true,
        sessions: vec![clean],
    };
    let destination = Report {
        compatible: false,
        sessions: vec![Row {
            child: true,
            unavailable: Some("checkpoint could not be decoded".into()),
            ..row("child")
        }],
    };
    assert!(finish(Some(&source), &destination).is_err());
}
