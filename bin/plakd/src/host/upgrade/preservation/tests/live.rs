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
         INSERT INTO identity_bindings VALUES('item','queue','root','removed','deleted',NULL);
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

#[test]
fn predecessor_requires_a_queue_removal_for_the_same_owner() {
    let f = Fixture::new();
    let before = predecessor(&f);
    f.product
        .execute(
            "UPDATE queue_items SET text='rewritten' WHERE item_id='item'",
            [],
        )
        .unwrap();
    f.refuses(&before, "regressed its revision");
    f.product
        .execute(
            "UPDATE queue_items SET revision=revision+1 WHERE item_id='item'",
            [],
        )
        .unwrap();
    f.compare(&before).unwrap();
    f.product
        .execute_batch(
            "DELETE FROM queue_items WHERE item_id='item';
        INSERT INTO identity_bindings VALUES('item','queue','another','removed','deleted',NULL);",
        )
        .unwrap();
    f.refuses(&before, "without a recorded removal");
    f.product
        .execute(
            "UPDATE identity_bindings SET owner_session_id='root' WHERE identity='item'",
            [],
        )
        .unwrap();
    f.compare(&before).unwrap();
}

#[test]
fn predecessor_requires_durable_tree_rows_or_a_new_owner_retirement() {
    let f = Fixture::new();
    let tree = Connection::open(f.paths.subagents_database()).unwrap();
    tree.execute_batch("
        INSERT INTO agent_trees VALUES('tree','tree',2,1,'live','single_lease','selection','checkout','{}',1,NULL);
        INSERT INTO agent_nodes (tree_id,agent_id,depth,description,prompt,output_schema_json,
             schema_digest,tuple_id,account,status,workspace_id,tier,path,active_turn,epoch_no)
          VALUES ('tree','child',1,'','','{}','','','', 'completed','w','read_only','.',0,0);
        INSERT INTO agent_attempts (tree_id,attempt_id,agent_id,status,turn_token,submit_harness_nudge,
             started_at,elapsed_ms,turns,tool_calls,input_tokens,output_tokens,output_schema_json,
             schema_digest,applied_terminal_read_seqs)
          VALUES ('tree','attempt','child','completed',1,0,0,0,1,0,0,0,'{}','','[]');
        INSERT INTO agent_candidates (tree_id,candidate_id,workspace_id,producing_agent_id,
             producing_attempt_id,base_commit,base_content,commit_id,content,sealed_at,
             adopted_evidence,kind,conflict_paths)
          VALUES ('tree','candidate','w','child','attempt','base','base','head','head',0,'{}','work','[]');
        INSERT INTO agent_commands VALUES('tree','command','child',NULL,'wake','text','[]',0,NULL,NULL,NULL);
        INSERT INTO agent_operations (tree_id,operation_id,caller_agent_id,tool_name,canonical_request,request_digest)
          VALUES ('tree','operation','child','message_agent','{}','hash'),
                 ('tree','stage','child','vcs_stage','{}','hash');").unwrap();
    let before = predecessor(&f);
    // Commit each fault so the read-only snapshot can see it. Restore the
    // exact row before trying the next table.
    for (table, predicate) in [
        ("agent_commands", "1"),
        ("agent_attempts", "1"),
        ("agent_candidates", "1"),
        ("agent_operations", "operation_id<>'stage'"),
        ("agent_nodes", "1"),
    ] {
        tree.execute_batch(&format!(
            "PRAGMA foreign_keys=OFF;
             CREATE TEMP TABLE saved AS SELECT * FROM {table} WHERE {predicate};
             DELETE FROM {table} WHERE {predicate};"
        ))
        .unwrap();
        f.refuses(&before, &format!("lost a durable {table} record"));
        tree.execute_batch(&format!(
            "INSERT INTO {table} SELECT * FROM saved; DROP TABLE saved;"
        ))
        .unwrap();
    }
    let summary = json!({"parent":"root","depth":1,"attempt":"attempt","status":"completed",
        "ended_at":1,"mail":null,"next_message_sequence":1,"result":null,"output":null,
        "description":"fixture"})
    .to_string();
    tree.execute_batch(
        "DELETE FROM agent_operations WHERE operation_id='stage';
        DELETE FROM agent_candidates;",
    )
    .unwrap();
    let retire = |agent: &str| {
        tree.execute(
            "INSERT INTO agent_retired_children VALUES ('tree',?1,?2)",
            params![agent, summary],
        )
        .unwrap();
    };
    retire("different-child");
    f.refuses(&before, "lost a durable agent_candidates record");
    retire("child");
    f.compare(&before).unwrap();
    // A seal that survived an earlier retirement is still required.
    tree.execute("INSERT INTO agent_candidates (tree_id,candidate_id,workspace_id,producing_agent_id,
             producing_attempt_id,base_commit,base_content,commit_id,content,sealed_at,
             adopted_evidence,kind,conflict_paths)
          VALUES ('tree','survivor','w','child','attempt','base','base','head','head',0,'{}','work','[]')", []).unwrap();
    let after_retirement = predecessor(&f);
    tree.execute("DELETE FROM agent_candidates", []).unwrap();
    f.refuses(&after_retirement, "lost a durable agent_candidates record");
}
