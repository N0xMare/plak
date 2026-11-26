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

mod gating {
    use super::*;
    use crate::command::BindingScope;
    use crate::commands::session::check_binding::inspect_with_dispositions;
    use std::collections::{BTreeSet, HashMap};
    use std::os::unix::fs::PermissionsExt as _;

    fn seeded(directory: &Path) -> PathBuf {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/hosting/hosted/tests/fixtures/current-bindings/runtime.sqlite3");
        let runtime = directory.join("runtime.sqlite3");
        std::fs::copy(fixture, &runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o600)).unwrap();
        runtime
    }

    fn heads(runtime: &Path) -> Vec<(String, String)> {
        let (report, _) =
            inspect_with_dispositions(runtime, HashMap::new(), HashMap::new(), BindingScope::All)
                .unwrap();
        let mut heads: Vec<_> = report
            .sessions
            .iter()
            .map(|row| (location(row), row.checkpoint.clone()))
            .collect();
        heads.sort();
        heads
    }

    /// The `<session>/<branch>` key product and tree dispositions use.
    fn location(row: &Row) -> String {
        format!("{}/{}", row.session, row.branch)
    }

    fn corrupt(runtime: &Path, checkpoint: &str) {
        let connection = rusqlite::Connection::open(runtime).unwrap();
        let changed = connection
            .execute(
                "UPDATE runtime_object_bytes SET bytes=x'00' WHERE id IN (
                    SELECT o.id FROM runtime_objects o JOIN checkpoints c
                    ON (o.kind,o.codec,o.digest)=(c.root_kind,c.root_codec,c.root_digest)
                    WHERE c.checkpoint_id=?1
                )",
                [checkpoint],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }

    fn keys(rows: &[&Row]) -> BTreeSet<String> {
        rows.iter().map(|row| binding_key(row)).collect()
    }

    fn json(rows: impl IntoIterator<Item = impl Serialize>) -> Vec<serde_json::Value> {
        rows.into_iter()
            .map(|row| serde_json::to_value(row).unwrap())
            .collect()
    }

    /// Archived, superseded and closed rows regress too, but neither scope
    /// lets them gate. The gating scope decodes only the rows that can.
    #[test]
    fn the_gating_scan_gives_the_full_scan_verdicts() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = seeded(directory.path());
        let heads = heads(&runtime);
        assert!(heads.len() >= 10, "{heads:?}");
        let [
            product_archived,
            product_superseded,
            runtime_archived,
            closed_child,
            archived_tree_child,
            live_child,
            live_root,
            ..,
        ] = heads.as_slice()
        else {
            unreachable!();
        };
        let mut rows = HashMap::new();
        rows.insert(product_archived.0.clone(), (true, false));
        rows.insert(product_superseded.0.clone(), (false, true));
        rows.insert(archived_tree_child.0.clone(), (true, false));
        let mut children = HashMap::new();
        children.insert(closed_child.0.clone(), true);
        children.insert(archived_tree_child.0.clone(), false);
        children.insert(live_child.0.clone(), false);
        let (session, _) = runtime_archived.0.split_once('/').unwrap();
        let connection = rusqlite::Connection::open(&runtime).unwrap();
        connection
            .execute(
                "UPDATE sessions SET archived=1 WHERE session_id=?1",
                [session],
            )
            .unwrap();
        drop(connection);
        let decoding = |scope| {
            inspect_with_dispositions(&runtime, rows.clone(), children.clone(), scope).unwrap()
        };
        let report = |scope| decoding(scope).0;
        let (full_source, gating_source) =
            (report(BindingScope::All), report(BindingScope::Gating));
        for (_, checkpoint) in [
            product_archived,
            product_superseded,
            runtime_archived,
            closed_child,
            archived_tree_child,
            live_child,
            live_root,
        ] {
            corrupt(&runtime, checkpoint);
        }
        let (full, decoded) = decoding(BindingScope::All);
        assert_eq!(decoded.len(), heads.len());
        let (gating, decoded) = decoding(BindingScope::Gating);
        // Non-gating heads are never decoded, whichever flag excludes them.
        assert_eq!(decoded.len(), heads.len() - 5);
        for (key, _) in [
            product_archived,
            product_superseded,
            runtime_archived,
            closed_child,
            archived_tree_child,
        ] {
            assert!(!decoded.contains(key), "{key} was decoded");
        }

        assert_eq!(full.sessions.len(), heads.len());
        assert_eq!(gating.sessions.len(), heads.len() - 5);
        // The non-gating rows really did regress in the full audit.
        for (key, _) in [
            product_archived,
            product_superseded,
            runtime_archived,
            closed_child,
            archived_tree_child,
        ] {
            let row = full
                .sessions
                .iter()
                .find(|row| location(row) == *key)
                .unwrap();
            assert!(!comparable(row) && row.audit_failed(), "{key}");
        }
        assert_eq!(
            json(gating.sessions.iter()),
            json(full.sessions.iter().filter(|row| comparable(row)))
        );
        assert_eq!(
            (full_source.compatible, full.compatible),
            (gating_source.compatible, gating.compatible)
        );
        assert!(full_source.compatible && !full.compatible);
        let expected: BTreeSet<_> = full
            .sessions
            .iter()
            .filter(|row| [&live_child.0, &live_root.0].contains(&&location(row)))
            .map(binding_key)
            .collect();
        assert_eq!(expected.len(), 2);
        for baseline in [Some(&full_source), None] {
            let full_baseline = Baseline::from_report(baseline);
            let gating_baseline = Baseline::from_report(baseline.map(|_| &gating_source));
            assert_eq!(full_baseline.0, gating_baseline.0);
            let refused = keys(&regressions(&full_baseline, &full));
            assert_eq!(refused, keys(&regressions(&gating_baseline, &gating)));
            if baseline.is_some() {
                assert_eq!(refused, expected);
            }
        }
        assert!(finish(Some(&full_source), &full).is_err());
        assert!(finish(Some(&gating_source), &gating).is_err());
    }
}

mod gating_fallback {
    use std::os::unix::process::ExitStatusExt as _;

    fn output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn only_an_unknown_argument_usage_refusal_falls_back_to_the_full_audit() {
        let unknown = "plakd: unknown option or unexpected argument; use `plakd session --help`\n";
        assert!(super::super::predates_gating(&output(2, "", unknown)));
        // A refused inspection still prints its report and exits 1.
        let report = r#"{"compatible":false,"sessions":[]}"#;
        assert!(!super::super::predates_gating(&output(
            1,
            report,
            "plakd: binding preflight refused; no runtime or state was changed\n"
        )));
        assert!(!super::super::predates_gating(&output(0, report, "")));
        // Other usage errors and any output on stdout are not the old image.
        assert!(!super::super::predates_gating(&output(
            2,
            "",
            "plakd: run requires --account\n"
        )));
        assert!(!super::super::predates_gating(&output(2, report, unknown)));
    }
}
