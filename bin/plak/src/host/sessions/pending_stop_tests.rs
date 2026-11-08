use super::*;

struct NoAdmission;

#[test]
fn idle_debt_older_than_the_newest_256_rows_is_not_hidden() {
    use plakous_hosted::sessions::projector::{
        RootInterrupt, RootInterruptCause, RootLossIntent, RootStopAnchor,
    };
    let (_temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let services = Services::for_home(paths.clone());
    let mut store = ProductStore::open(&paths).unwrap();
    let mut old = crate::store::sessions::SessionRecord::new(paths.home().to_owned());
    old.updated_at_unix = 1;
    old.runner = Some(plakous_hosted::sessions::RunnerDisplay {
        name: "offline".into(),
        runner_id: "offline-id".into(),
        workspace_root: paths.home().to_owned(),
        workspace_cwd: paths.home().to_owned(),
    });
    store.commit(&mut old).unwrap();
    store
        .write_root_loss_at_cursor(
            &old.id,
            &RootLossIntent::Interrupt(RootInterrupt {
                anchor: RootStopAnchor::Turn {
                    turn_id: "old-handoff".into(),
                },
                cause: RootInterruptCause::Handoff,
            }),
        )
        .unwrap();
    for number in 0..256 {
        let mut recent = crate::store::sessions::SessionRecord::new(paths.home().to_owned());
        recent.updated_at_unix = number + 2;
        store.commit(&mut recent).unwrap();
    }
    assert!(
        !store
            .directory(None)
            .unwrap()
            .iter()
            .any(|row| row.id == old.id)
    );
    assert!(store.idle_debt().unwrap().0);
    let product = ProductSessions::new(&services, None, None).unwrap();
    assert_eq!(product.owed().unwrap(), vec![old.id.clone()]);
    store
        .write_root_loss_at_cursor(
            &old.id,
            &RootLossIntent::Interrupt(RootInterrupt {
                anchor: RootStopAnchor::Turn {
                    turn_id: "old-handoff".into(),
                },
                cause: RootInterruptCause::Stopped,
            }),
        )
        .unwrap();
    assert!(!store.idle_debt().unwrap().0);
    // Only the recorded stop, not an ordinary offline placement, blocks exit.
    assert!(product.durable_idle_exit_blocked());
}

#[test]
fn an_offline_runner_without_recorded_stop_debt_does_not_pin_the_host() {
    let (_temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let services = Services::for_home(paths.clone());
    let mut store = ProductStore::open(&paths).unwrap();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_owned());
    record.runner = Some(plakous_hosted::sessions::RunnerDisplay {
        name: "offline".into(),
        runner_id: "offline-id".into(),
        workspace_root: paths.home().to_owned(),
        workspace_cwd: paths.home().to_owned(),
    });
    store.commit(&mut record).unwrap();
    assert_eq!(store.idle_debt().unwrap(), (false, Vec::new()));
    let product = ProductSessions::new(&services, None, None).unwrap();
    assert!(!product.durable_idle_exit_blocked());
}

#[test]
fn archived_stop_is_a_read_only_noop_even_with_an_unreadable_runtime() {
    let (_temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let services = Services::for_home(paths.clone());
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_owned());
    record.archived = true;
    services
        .session_records()
        .unwrap()
        .save(&mut record)
        .unwrap();
    std::fs::write(paths.runtime_database(), b"not a runtime database").unwrap();
    let product = ProductSessions::new(&services, None, None).unwrap();
    product.stop_dormant(&record.id).unwrap();
    assert!(!product.runner_pending(&record.id));
    assert_eq!(
        services
            .session_records()
            .unwrap()
            .load_intents(&record.id)
            .unwrap()
            .root_interrupts,
        []
    );
    assert_eq!(
        std::fs::read(paths.runtime_database()).unwrap(),
        b"not a runtime database"
    );
}

impl Placement for NoAdmission {
    fn validate(
        &self,
        _: &plakous_host_protocol::wire::RunnerLabel,
        _: &str,
    ) -> Result<placement::Validated, RequestRefusal> {
        panic!("a dormant stop attempted placement admission");
    }
}

#[test]
fn dormant_stop_is_durable_without_admitting_an_unready_or_removed_runner() {
    for remote in [false, true] {
        let (temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let services = Services::for_home(paths.clone());
        let workspace = temporary.path().join("workspace-not-present");
        let mut record = crate::store::sessions::SessionRecord::new(workspace.clone());
        if remote {
            record.runner = Some(plakous_hosted::sessions::RunnerDisplay {
                name: "removed".into(),
                runner_id: "removed-id".into(),
                workspace_root: workspace.clone(),
                workspace_cwd: workspace,
            });
        } else {
            services
                .runners()
                .configure(plakous::runner::ConfiguredRunner {
                    runner_id: "local".into(),
                    names: vec!["local".into()],
                });
        }
        let records = services.session_records().unwrap();
        records.save(&mut record).unwrap();
        let mut product = ProductSessions::new(&services, None, None).unwrap();
        product.placement = Box::new(NoAdmission);
        product.stop_dormant(&record.id).unwrap();
        assert!(product.runner_pending(&record.id));
        let saved = records.load(&record.id).unwrap();
        assert!(saved.child_wake_suppressed);
        assert!(saved.plakous_locator.is_none(), "the stop opened a driver");
        assert_eq!(
            records
                .load_intents(&record.id)
                .unwrap()
                .root_interrupts
                .last()
                .unwrap()
                .value
                .cause,
            plakous_hosted::sessions::projector::RootInterruptCause::Stopped,
        );
    }
}
