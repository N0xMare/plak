use super::*;

fn predecessor(f: &Fixture) -> Snapshot {
    Snapshot::capture_at(&f.paths, None, Evidence::BaselinePredatesDrain)
        .unwrap()
        .0
}

fn child(f: &Fixture) {
    f.product
        .execute_batch(
            "UPDATE sessions SET node_agent_id=node_tree_id WHERE id='root';
         INSERT INTO sessions
           (id,revision,create_operation_id,workspace_root,workspace_identity,created_at,
            updated_at,node_tree_id,node_agent_id,archived)
         VALUES('child',1,'new-child','workspace','workspace',1,1,'tree','child',0);",
        )
        .unwrap();
}

#[tokio::test]
async fn a_pre_drain_baseline_allows_live_activity_and_recorded_deletion() {
    let f = Fixture::with_stub_runtime(false);
    child(&f);
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(f.paths.runtime_database()).unwrap()).unwrap();
    let working = native_session_with_id(&sessions, f.temp.path(), "preservation:working").await;
    let removed = native_session_with_id(&sessions, f.temp.path(), "preservation:removed").await;
    let working_head = working.locator();
    let removed_head = removed.locator();
    let working_id = working_head.session_id().to_string();
    working
        .agent()
        .start(
            CommandId::new("preservation:initial").unwrap(),
            "first turn",
        )
        .await
        .unwrap()
        .into_turn()
        .unwrap()
        .result()
        .await
        .unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let tree = Connection::open(f.paths.subagents_database()).unwrap();
    tree.execute(
        "INSERT INTO agent_trees VALUES(?1,?1,1,1,'live','single_lease','selection','checkout','{}',0,NULL)",
        [&tree_id],
    ).unwrap();
    f.product
        .execute(
            "INSERT INTO sessions
           (id,revision,create_operation_id,workspace_root,workspace_identity,created_at,
            updated_at,locator,node_tree_id,node_agent_id,archived)
         VALUES(?1,1,'removed','workspace','workspace',1,1,?2,?1,?1,0)",
            params![tree_id, removed_head.to_string()],
        )
        .unwrap();
    assert_eq!(
        f.runtime
            .execute(
                "INSERT INTO line_job_terminals SELECT line_id,generation,1,x'00' FROM lines
         WHERE head_session=?1",
                [&working_id],
            )
            .unwrap(),
        1
    );
    let before = predecessor(&f);
    f.product
        .execute_batch(
            "UPDATE sessions SET updated_at=20,revision=2 WHERE id='child';
         UPDATE sessions SET archived=1, model='model', effort='medium', account='account',
            revision=revision+1 WHERE id='root';
         DELETE FROM queue_items WHERE item_id='item';
         INSERT INTO sessions
           (id,revision,create_operation_id,workspace_root,workspace_identity,created_at,
            updated_at,node_tree_id,node_agent_id,archived)
         VALUES('unstarted',1,'unstarted','workspace','workspace',1,1,'unstarted','unstarted',0);",
        )
        .unwrap();
    // Fold an ordinary feed trim into its retained window, closing only
    // the oldest record. The overlapping admitted prefix must survive.
    assert_eq!(
        f.runtime
            .execute(
                "UPDATE sessions SET feed_retained_from=2,feed_bytes=feed_bytes-
             (SELECT length(record) FROM session_feed WHERE session_id=?1 AND seq=1)
             WHERE session_id=?1 AND feed_retained_from=1 AND feed_tail>1",
                [&working_id],
            )
            .unwrap(),
        1
    );
    assert_eq!(
        f.runtime
            .execute(
                "DELETE FROM session_feed WHERE session_id=?1 AND seq=1",
                [&working_id],
            )
            .unwrap(),
        1
    );
    f.runtime.execute(
        "DELETE FROM line_job_terminals WHERE line_id IN (SELECT line_id FROM lines WHERE head_session=?1)",
        [&working_id],
    ).unwrap();
    working
        .agent()
        .start(
            CommandId::new("preservation:advance").unwrap(),
            "another turn",
        )
        .await
        .unwrap()
        .into_turn()
        .unwrap()
        .result()
        .await
        .unwrap();
    let latest = working.locator();
    working.release().await.unwrap();
    removed.release().await.unwrap();
    sessions.archive(latest).await.unwrap();
    // Advance uses an atomic generation-bound head update and consumes the
    // old generation's pending terminal rows. This fixture checks those rows,
    // not the separate runtime API which validates the materialized ledger.
    assert_eq!(
        f.runtime
            .execute(
                "UPDATE lines SET head_session=?1,head_branch=?2,generation=x'0000000000000002'
         WHERE head_session=?1",
                params![&working_id, working_head.branch_id().to_string()],
            )
            .unwrap(),
        1
    );
    let archived = sessions.archive(removed_head).await.unwrap();
    sessions
        .delete(removed_head.session_id(), archived.archived_revision())
        .await
        .unwrap();
    // Maintenance can already have swept the deleted tree inventory.
    // The owning root's native deletion receipt still proves the delete.
    tree.execute("DELETE FROM agent_trees WHERE tree_id=?1", [&tree_id])
        .unwrap();
    f.product
        .execute("DELETE FROM sessions WHERE id=?1", [&tree_id])
        .unwrap();
    f.compare(&before).unwrap();

    f.runtime
        .execute(
            "UPDATE session_feed SET record=x'00' WHERE session_id=?1 AND seq=2",
            [&working_id],
        )
        .unwrap();
    f.refuses(&before, "admitted runtime feed prefix");
}

#[test]
fn both_evidence_levels_refuse_unrecorded_loss_of_a_non_head_child() {
    let f = Fixture::new();
    child(&f);
    let held = f.before();
    let early = predecessor(&f);
    assert_eq!(
        f.product
            .execute("DELETE FROM sessions WHERE id='child'", [])
            .unwrap(),
        1
    );
    for before in [&held, &early] {
        f.refuses(before, "lost or changed a product session");
    }
}

#[test]
fn predecessor_refuses_an_unsealed_runtime_disappearance_and_tree_regression() {
    let f = Fixture::new();
    let tree = Connection::open(f.paths.subagents_database()).unwrap();
    tree.execute_batch(
        "INSERT INTO agent_trees VALUES('tree','tree',2,1,'live','single_lease','selection','checkout','{}',1,NULL);",
    ).unwrap();
    let before = predecessor(&f);
    tree.execute("UPDATE agent_trees SET revision=1", [])
        .unwrap();
    f.refuses(&before, "regressed its revision");
    tree.execute("UPDATE agent_trees SET revision=2", [])
        .unwrap();
    f.runtime
        .execute("DELETE FROM sessions WHERE session_id=?1", [SESSION])
        .unwrap();
    f.refuses(&before, "without a sealed delete");
}
