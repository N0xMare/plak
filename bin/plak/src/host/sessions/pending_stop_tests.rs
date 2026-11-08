use super::*;

struct NoAdmission;

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
    assert!(
        services
            .session_records()
            .unwrap()
            .load_intents(&record.id)
            .unwrap()
            .root_interrupts
            .is_empty()
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
