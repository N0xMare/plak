use super::linked::{connect, open, service};
use super::*;

async fn cleanup(service: &plakous::runner::RunnerService) {
    service.operator_terminals().unwrap().close_all();
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while service.operator_terminals().unwrap().has_live_terminals() {
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn operator_host_same_instance_inventory_repins_a_live_terminal_and_residency() {
    let (directory, registry, count) = fixture();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let service = service(&workspace);
    let (client, serving) = connect(&registry, &service).await;
    registry.reconcile("local", &client).await.unwrap();
    let view = open(&registry, &count, &context("operator"), &workspace).await;
    registry.release_proven(&view.handle.identity).unwrap();
    assert_eq!(count.get(), 0);
    assert!(!registry.blocks_idle_exit());
    assert!(service.operator_terminals().unwrap().has_live_terminals());
    registry.reconcile("local", &client).await.unwrap();
    {
        let state = registry.lock();
        let entry = &state.entries[&view.handle.identity];
        assert!(entry.pinned);
        assert!(entry.hold.is_some());
    }
    assert_eq!(count.get(), 1);
    assert!(registry.blocks_idle_exit());
    assert!(registry.admit_local("local", &workspace).is_err());
    cleanup(&service).await;
    registry.reconcile("local", &client).await.unwrap();
    assert_eq!(count.get(), 0);
    client.disconnect();
    serving.await.unwrap();
}

#[tokio::test]
async fn operator_host_acknowledged_unbound_loss_waits_for_deadline_and_resets_on_rebind() {
    let (directory, registry, count) = fixture();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let service = service(&workspace);
    let (client, serving) = connect(&registry, &service).await;
    registry.reconcile("local", &client).await.unwrap();
    let owner = context("operator");
    let view = open(&registry, &count, &owner, &workspace).await;
    let revision = client.connection_revision();
    assert!(!revision.is_unbound());
    client.disconnect();
    serving.await.unwrap();
    assert!(revision.is_unbound());
    assert!(matches!(
        registry.close(&owner, &view.handle, true).await,
        Err(TerminalError::Unavailable)
    ));
    age_unbound(&registry);
    let (host, remote) = tokio::io::duplex(64 * 1024);
    let runner = service.clone();
    let serving = tokio::spawn(async move {
        runner.serve_connection(remote).await;
    });
    client.reconnect(host).await.unwrap();
    assert!(
        !revision.is_unbound(),
        "a later connection invalidates the old loss witness"
    );
    client.disconnect();
    serving.await.unwrap();
    assert!(!revision.is_unbound());
    assert!(matches!(
        registry.close(&owner, &view.handle, true).await,
        Err(TerminalError::Unavailable)
    ));
    age_unbound(&registry);
    assert!(matches!(
        registry.close(&owner, &view.handle, false).await,
        Err(TerminalError::Unavailable)
    ));
    assert_eq!(count.get(), 1, "elapsed time alone never acknowledges loss");
    let lost = registry.close(&owner, &view.handle, true).await.unwrap();
    assert!(lost.lost);
    assert_eq!(count.get(), 0);
    assert!(!registry.blocks_idle_exit());
    assert!(
        service.operator_terminals().unwrap().has_live_terminals(),
        "acknowledgment is not death proof"
    );
    cleanup(&service).await;
}

fn age_unbound(registry: &TerminalRegistry) {
    registry.lock().unbound.get_mut("local").unwrap().since = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(91))
        .unwrap();
}

#[tokio::test]
async fn operator_host_acknowledged_removed_runner_releases_pin_without_claiming_cleanup() {
    use plakous::runner::protocol::operator::TerminalDimensions;
    let (directory, registry, count) = fixture();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let service = service(&workspace);
    let (client, serving) = connect(&registry, &service).await;
    registry.reconcile("retired", &client).await.unwrap();
    let owner = context("operator");
    let view = registry
        .open_placement(
            &owner,
            pin(&registry, workspace.clone()).key,
            TerminalDimensions::new(24, 80).unwrap(),
            control::Placement {
                runner: "retired".into(),
                workspace,
            },
            count.hold().unwrap(),
        )
        .await
        .unwrap();
    client.disconnect();
    serving.await.unwrap();
    assert!(matches!(
        registry.close(&owner, &view.handle, false).await,
        Err(TerminalError::Unavailable)
    ));
    assert_eq!(count.get(), 1);
    let lost = registry.close(&owner, &view.handle, true).await.unwrap();
    assert!(lost.lost);
    assert_eq!(count.get(), 0);
    assert!(!registry.blocks_idle_exit());
    assert!(service.operator_terminals().unwrap().has_live_terminals());
    cleanup(&service).await;
}
