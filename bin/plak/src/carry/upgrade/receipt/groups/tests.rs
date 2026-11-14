use super::*;
use crate::store::sessions::SessionRecord;

#[test]
fn move_group_receipt_lock_excludes_a_second_disposition_writer() {
    let home = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home.path().to_path_buf());
    let lock = super::super::publication::lock(&paths).unwrap();
    let second = std::fs::File::open(super::super::root(&paths).join("publication.lock")).unwrap();
    assert!(second.try_lock().is_err());
    drop(lock);
    assert!(second.try_lock().is_ok());
}

#[test]
fn move_group_prior_intents_are_exact_bounded_and_never_future_or_foreign() {
    let record = SessionRecord::new(std::path::PathBuf::from("/workspace"));
    let mut receipt = grouped(&record);
    receipt.phase = Phase::Committed;
    let group = &mut receipt.groups[0];
    group.phase = GroupPhase::Published;
    group.target_digest = Some("3".repeat(64));
    let line = group.lines.iter().next().unwrap().clone();
    let to = plakous::runner::protocol::LineOwner {
        line: line.clone(),
        generation: 2,
        branch: uuid::Uuid::new_v4().to_string(),
        fence: 10,
    };
    let prior = plakous::runner::protocol::LineOwner {
        fence: 8,
        ..to.clone()
    };
    group.transfers.insert(
        line.clone(),
        OwnerTransfer {
            from: None,
            to: to.clone(),
            previous: vec![prior.clone()],
        },
    );
    receipt.validate().unwrap();
    for invalid in [
        vec![plakous::runner::protocol::LineOwner {
            generation: 1,
            ..prior.clone()
        }],
        vec![plakous::runner::protocol::LineOwner {
            branch: uuid::Uuid::new_v4().to_string(),
            ..prior.clone()
        }],
        vec![plakous::runner::protocol::LineOwner {
            line: uuid::Uuid::new_v4().to_string(),
            ..prior.clone()
        }],
        vec![plakous::runner::protocol::LineOwner {
            fence: 0,
            ..prior.clone()
        }],
        vec![to],
        vec![prior.clone(), prior.clone()],
        vec![prior; 65],
    ] {
        receipt.groups[0].transfers.get_mut(&line).unwrap().previous = invalid;
        assert!(receipt.validate().is_err());
    }
}

fn grouped(record: &SessionRecord) -> Receipt {
    let mut receipt = super::super::tests::receipt(record, true);
    receipt.groups.push(Group {
        id: format!("group-{}", "1".repeat(64)),
        operation: [1; 32],
        trees: [record.node.tree_id.clone()].into(),
        products: [record.id.clone()].into(),
        sessions: [uuid::Uuid::new_v4().to_string()].into(),
        lines: [uuid::Uuid::new_v4().to_string()].into(),
        runners: ["local".to_owned()].into(),
        source_digest: Some("2".repeat(64)),
        source_retired: false,
        target_digest: None,
        phase: GroupPhase::Pending,
        transfers: BTreeMap::default(),
        abandon_error: None,
        abandon_retry_after: None,
    });
    receipt
}

#[test]
fn move_group_never_publishes_one_member_or_acknowledges_one_pending_tree() {
    let first = SessionRecord::new(std::path::PathBuf::from("/workspace"));
    let second = SessionRecord::new(std::path::PathBuf::from("/workspace"));
    let mut receipt = grouped(&first);
    receipt
        .mappings
        .extend(super::super::tests::receipt(&second, true).mappings);
    receipt.groups[0].products.insert(second.id.clone());
    receipt.groups[0].trees.insert(second.node.tree_id);
    receipt.phase = Phase::Committed;
    receipt.validate().unwrap();
    receipt.mappings[0].ready = true;
    assert!(receipt.validate().is_err());
    receipt.groups[0].phase = GroupPhase::Ready;
    receipt.groups[0].source_retired = true;
    receipt.groups[0].target_digest = Some("3".repeat(64));
    assert!(receipt.validate().is_err());
    receipt.mappings[1].ready = true;
    receipt.validate().unwrap();
    receipt.phase = Phase::Acknowledged;
    receipt.validate().unwrap();
    receipt.mappings[0].ready = false;
    assert!(receipt.validate().is_err());
}

#[test]
fn move_group_abandonment_keeps_local_reservations_invisible_forever() {
    let home = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home.path().to_path_buf());
    let record = SessionRecord::new(home.path().join("workspace"));
    let mut receipt = grouped(&record);
    receipt.phase = Phase::Acknowledged;
    receipt.groups[0].phase = GroupPhase::Abandoned;
    receipt.mappings[0].abandoned = true;
    receipt.save(&paths).unwrap();
    assert!(super::super::ensure_open(&paths, &record).is_err());
    assert!(
        super::super::withheld_targets(&paths)
            .unwrap()
            .contains(&record.id)
    );
    #[cfg(feature = "web")]
    assert_eq!(super::super::withheld_lines(&paths).unwrap().len(), 1);
}

#[test]
fn move_group_membership_cannot_overlap_or_omit_a_reserved_root() {
    let record = SessionRecord::new(std::path::PathBuf::from("/workspace"));
    let mut receipt = grouped(&record);
    receipt.validate().unwrap();
    let encoded = serde_json::to_value(&receipt.groups[0]).unwrap();
    let mut second: Group = serde_json::from_value(encoded).unwrap();
    second.operation = [2; 32];
    second.id = format!("group-{}", "2".repeat(64));
    receipt.groups.push(second);
    assert!(receipt.validate().is_err());
    receipt.groups.pop();
    receipt.groups[0].products.clear();
    assert!(receipt.validate().is_err());
}

#[test]
fn move_group_abandon_guard_errors_are_visible_and_back_off_automatic_retry() {
    let root = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(root.path().to_path_buf());
    let record = SessionRecord::new(std::path::PathBuf::from("/workspace"));
    let mut receipt = grouped(&record);
    receipt.groups[0].phase = GroupPhase::Abandoning;
    receipt.phase = Phase::Committed;
    receipt.groups[0].abandon_error = Some("execution was opened".into());
    receipt.groups[0].abandon_retry_after = Some(u64::MAX);
    receipt.save(&paths).unwrap();
    assert!(!receipt.groups[0].abandon_retry_due());
    assert!(super::super::abandoning(&paths).unwrap()[0].contains("execution was opened"));
    receipt.groups[0].abandon_retry_after = Some(0);
    assert!(receipt.groups[0].abandon_retry_due());
}

#[test]
fn move_aborted_receipt_never_withholds_a_retried_identity() {
    let home = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home.path().to_path_buf());
    let record = SessionRecord::new(home.path().join("workspace"));
    let mut authority = grouped(&record);
    authority.phase = Phase::Aborted;
    authority.save(&paths).unwrap();
    assert!(super::super::ensure_open(&paths, &record).is_ok());
    assert!(super::super::withheld_targets(&paths).unwrap().is_empty());
    assert!(super::super::withheld_lines(&paths).unwrap().is_empty());
}

#[cfg(feature = "web")]
#[test]
fn move_abandon_requires_the_explicit_group_for_linked_trees() {
    let first = SessionRecord::new("/workspace".into());
    let second = SessionRecord::new("/workspace".into());
    let mut authority = grouped(&first);
    authority.groups[0]
        .trees
        .insert(second.node.tree_id.clone());
    authority.groups[0].products.insert(second.id);
    let select = crate::carry::host_move::disposition::select;
    assert!(select(&authority, &first.id).is_err());
    assert!(select(&authority, &second.node.tree_id).is_err());
    assert_eq!(
        select(&authority, &authority.groups[0].id).unwrap(),
        Some(0)
    );
    assert_eq!(select(&authority, "unrelated").unwrap(), None);
}

#[test]
fn move_three_key_lookup_ignores_surrendered_copies_but_not_an_unsurrendered_source() {
    for committed in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let paths = crate::home::paths::PlakPaths::explicit(home.path().to_path_buf());
        let record = SessionRecord::new(home.path().join("workspace"));
        let mut first = grouped(&record);
        first.source_key = "r1.e1.p1.t1".into();
        first.destination_key = "r2.e2.p2.t2".into();
        first.phase = if committed {
            Phase::Acknowledged
        } else {
            Phase::Prepared
        };
        first.mappings[0].ready = committed;
        first.groups[0].source_retired = committed;
        first.groups[0].phase = if committed {
            GroupPhase::Ready
        } else {
            GroupPhase::Pending
        };
        first.groups[0].target_digest = committed.then(|| "3".repeat(64));
        first.save(&paths).unwrap();
        let mut second = grouped(&record);
        second.source_key.clone_from(&first.destination_key);
        second.sequence = 2;
        second.phase = Phase::Acknowledged;
        second.groups[0].phase = GroupPhase::Ready;
        second.groups[0].source_retired = true;
        second.groups[0].target_digest = Some("4".repeat(64));
        second.mappings[0].ready = true;
        second.save(&paths).unwrap();
        for key in [&first.source_key, &second.source_key] {
            let directory = paths.state_root().join(key).join("sessions");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join(format!("{}.events.jsonl", record.id)), b"").unwrap();
        }
        let expected = (!committed).then(|| (first.source_key.clone(), record.id.clone()));
        assert_eq!(
            crate::home::state::foreign_session(&paths, &record.id),
            expected
        );
        let independent = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            paths
                .state_root()
                .join(&second.source_key)
                .join("sessions")
                .join(format!("{independent}.events.jsonl")),
            b"",
        )
        .unwrap();
        assert_eq!(
            crate::home::state::foreign_session(&paths, &independent),
            Some((second.source_key, independent))
        );
    }
}
