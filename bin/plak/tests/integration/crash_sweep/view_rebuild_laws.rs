use super::*;

const SHAPES: &[&str] = &[
    "busy_quit_escalates",
    "hosted_tool_call_final",
    "capacity_overflow",
    "final_only",
    "direct_tool_then_final",
    "promoted_background_job",
    "sealed_family_artifacts",
    "non_stop_finish_length",
    "non_stop_finish_refusal",
    "steer_accepted_mid_turn",
    "scheduled_compaction_in_turn",
    "scheduled_compaction_workflow",
    "code_mode_cell_nested_then_final",
    "code_mode_cell_final",
];

#[test]
fn full_sqlite_view_rebuild_and_plain_reopen_equal_incremental_for_every_sweep_shape() {
    for shape in SHAPES {
        let (seed, session) = setup(shape);
        let fixture = seed.fork(&shapes::script(shape));
        let outcome = lifecycle(&fixture, &session, shape, None, None);
        assert!(outcome.status.success(), "{shape}: {}", outcome.stderr);
        summary::read_converged(&fixture);
        let paths = summary::paths(&fixture);
        let incremental = view_snapshot::snapshot(&paths);
        let mut store = plak::store::ProductStore::open(&paths).unwrap();
        store.delete_views(&session).unwrap();
        drop(store);
        summary::read_converged(&fixture);
        assert_eq!(
            view_snapshot::snapshot(&paths),
            incremental,
            "{shape}: complete SQL views differ"
        );
        if *shape == "busy_quit_escalates" {
            // Compare identical canonical facts, not two independently minted recovery heads.
            let mut opened = reopen_result(&fixture, &session)
                .unwrap_or_else(|outcome| panic!("busy quit recovery refused: {}", outcome.stderr));
            summary::read_reopened(&fixture);
            opened.send(&quit_frame());
            assert!(opened.finish().status.success());
        }
        assert_plain_open_twin(&fixture, &session, shape, &shapes::script(shape));
    }
}

pub(super) fn assert_plain_open_twin(
    fixture: &Fixture,
    session: &str,
    shape: &str,
    script: &Value,
) {
    let intact = fixture.fork(script);
    let deleted = fixture.fork(script);
    let copied_bytes = |arm: &Fixture| {
        summary::database_bytes(arm)
            .into_iter()
            .map(|(path, bytes)| (path.strip_prefix(arm.home()).unwrap().to_owned(), bytes))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    assert_eq!(
        copied_bytes(&intact),
        copied_bytes(&deleted),
        "twins must start byte-identical"
    );
    let mut store = plak::store::ProductStore::open(&summary::paths(&deleted)).unwrap();
    store.delete_views(session).unwrap();
    assert!(store.load_views(session).unwrap().is_none());
    drop(store);
    let mut snapshots = Vec::new();
    // Compare at Ready: Quit is a separate mutation, not part of open.
    for arm in [&intact, &deleted] {
        let mut command = arm.command();
        command.args(["agent", "--session", session]);
        let mut opened = AgentProcess::start_command(command, true);
        opened.wait_for("the one plain reopen", |row| row["kind"] == "ready");
        composite_laws::assert_at_tail(arm, session);
        snapshots.push(view_snapshot::snapshot(&summary::paths(arm)));
        view_idempotence_tests::assert_idempotent(arm, session, shape);
        opened.send(&quit_frame());
        assert!(opened.finish().status.success(), "{shape}");
    }
    assert_eq!(
        snapshots[0], snapshots[1],
        "{shape}: one real open did not rebuild the complete views",
    );
}
