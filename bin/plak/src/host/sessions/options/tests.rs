use super::*;
use plakous::runner::{ConfiguredRunner, PlacementFailure, WorkspaceCatalog};
use std::sync::Arc;

#[test]
fn inadmissible_local_recents_are_omitted_without_losing_admitted_choices() {
    let (root, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let valid = root.path().join("workspace");
    std::fs::create_dir(&valid).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&valid, &alias).unwrap();
    let services = crate::shell::services::Services::for_home(paths.clone());
    for path in [
        &valid,
        paths.home(),
        root.path(),
        &root.path().join("missing"),
        &alias,
    ] {
        let mut record = crate::store::sessions::SessionRecord::new(path.to_owned());
        services
            .session_records()
            .unwrap()
            .save(&mut record)
            .unwrap();
    }
    for index in 0..25 {
        let mut record = crate::store::sessions::SessionRecord::new(
            root.path().join(format!("missing-{index}")),
        );
        record.updated_at_unix += 100 + index;
        services
            .session_records()
            .unwrap()
            .save(&mut record)
            .unwrap();
    }
    let prepared = crate::commands::runner::registry::prepare(&services).unwrap();
    assert_eq!(prepared.local.options().default_workspace, None);
    prepared
        .services
        .runners()
        .ready_local(prepared.local, 1)
        .unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let product =
        ProductSessions::new(&prepared.services, None, Some(runtime.handle().clone())).unwrap();
    let options = product.session_options(None, &mut |_| Ok(())).unwrap();
    let local = options
        .runners
        .iter()
        .find(|row| row.runner.runner_id == "local")
        .unwrap();
    assert_eq!(
        local
            .recent_workspaces
            .iter()
            .map(|recent| recent.path.as_str())
            .collect::<Vec<_>>(),
        [valid.to_str().unwrap()]
    );
    assert_eq!(local.default_workspace, None);
}

#[test]
fn cached_picker_rows_finish_once_profiles_stay_ungated_and_workspace_checks_stay_fresh() {
    let (root, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = std::fs::canonicalize(workspace).unwrap();
    let services = crate::shell::services::Services::for_home(paths.clone());
    let registry = services.runners();
    registry.configure(ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    registry.configure(ConfiguredRunner {
        runner_id: "unpaired-peer".into(),
        names: vec!["peer".into()],
    });
    registry.blocked("unpaired-peer", PlacementFailure::RunnerNotPaired, 1);
    registry
        .ready_local(
            Arc::new(WorkspaceCatalog::local(Some(&workspace), &[], paths.home()).unwrap()),
            1,
        )
        .unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let product = ProductSessions::new(&services, None, Some(runtime.handle().clone())).unwrap();
    let probe = wire::WorkspaceProbe {
        runner: wire::RunnerLabel::local(),
        path: workspace.to_str().unwrap().to_owned(),
    };
    for exists in [true, false] {
        if !exists {
            std::fs::remove_dir(&workspace).unwrap();
        }
        let mut completed = Vec::new();
        let options = product
            .session_options(Some(&probe), &mut |event| {
                if event.row.is_some() {
                    completed.push(event.runner.runner_id);
                }
                Ok(())
            })
            .unwrap();
        completed.sort();
        assert_eq!(completed, ["local", "unpaired-peer"]);
        assert_ne!(options.profiles.rows, []);
        assert!(options.runners.iter().any(|row| row.runner.name == "peer"
            && matches!(row.status, wire::RunnerStatus::Blocked { .. })));
        if exists {
            assert!(matches!(
                options.workspace,
                Some(wire::WorkspaceOptions::Valid { .. })
            ));
        } else {
            assert!(matches!(
                options.workspace,
                Some(wire::WorkspaceOptions::Refused {
                    refusal: wire::PlacementReason::WorkspaceRefused {
                        kind: wire::WorkspaceRefusalKind::NotFound,
                        ..
                    }
                })
            ));
        }
    }
}
