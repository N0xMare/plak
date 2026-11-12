use super::*;

fn receipt(record: &SessionRecord, destination: bool) -> Receipt {
    let own = crate::home::state::state_key();
    Receipt {
        version: 1,
        id: uuid::Uuid::new_v4(),
        sequence: 1,
        source_key: if destination {
            "r1.e1.p1.t1".into()
        } else {
            own.clone()
        },
        source_instance: plakous_recursive_subagents::tree::HostInstance::generate().to_string(),
        source_binary: "a".repeat(12),
        source_executable: PathBuf::from("/fixture/previous/plak"),
        host_argv: vec!["host".into()],
        destination_key: if destination {
            own
        } else {
            "r1.e1.p1.t1".into()
        },
        destination_binary: "b".repeat(12),
        destination: PathBuf::from("/fixture/bin/plak"),
        phase: Phase::Prepared,
        mappings: vec![Mapping {
            source: record.id.clone(),
            source_node: plakous_host_protocol::wire::NodeAddress {
                tree_id: record.node.tree_id.clone(),
                agent_id: record.node.agent_id.clone(),
            },
            successor: if destination {
                record.id.clone()
            } else {
                uuid::Uuid::new_v4().to_string()
            },
            digest: "c".repeat(64),
        }],
    }
}

#[test]
fn source_surrender_precedes_target_execution_admission() {
    for destination in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(home.path().to_path_buf());
        let record = SessionRecord::new(home.path().join("workspace"));
        let mut receipt = receipt(&record, destination);
        for phase in [
            Phase::Prepared,
            Phase::Committed,
            Phase::Acknowledged,
            Phase::Aborted,
        ] {
            receipt.phase = phase;
            receipt.save(&paths).unwrap();
            let expected = if destination {
                phase == Phase::Acknowledged
            } else {
                matches!(phase, Phase::Prepared | Phase::Aborted)
            };
            assert_eq!(
                ensure_open(&paths, &record).is_ok(),
                expected,
                "{phase:?}, destination={destination}"
            );
            assert_eq!(read(&receipt.path(&paths)).unwrap().phase, phase);
        }
    }
}

#[test]
fn an_unreadable_or_linked_receipt_never_grants_execution_authority() {
    let home = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(home.path().to_path_buf());
    let record = SessionRecord::new(home.path().join("workspace"));
    let receipt = receipt(&record, false);
    receipt.save(&paths).unwrap();
    let path = receipt.path(&paths);
    let kept = path.with_extension("kept");
    fs::rename(&path, &kept).unwrap();
    std::os::unix::fs::symlink(&kept, &path).unwrap();
    assert!(ensure_open(&paths, &record).is_err());
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"not a receipt").unwrap();
    assert!(ensure_open(&paths, &record).is_err());
}

#[test]
fn duplicate_mapping_and_unknown_receipt_versions_refuse_before_publication() {
    let home = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(home.path().to_path_buf());
    let record = SessionRecord::new(home.path().join("workspace"));
    let mut receipt = receipt(&record, false);
    receipt.version = 2;
    assert!(receipt.save(&paths).is_err());
    assert!(!root(&paths).exists());
    receipt.version = 1;
    receipt.mappings.push(Mapping {
        source: record.id.clone(),
        source_node: plakous_host_protocol::wire::NodeAddress {
            tree_id: record.node.tree_id.clone(),
            agent_id: record.node.agent_id.clone(),
        },
        successor: uuid::Uuid::new_v4().to_string(),
        digest: "c".repeat(64),
    });
    assert!(receipt.save(&paths).is_err());
    assert!(!root(&paths).exists());
}
