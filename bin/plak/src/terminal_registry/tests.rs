//! Host pins survive loss without becoming shell recovery or line authority.

use super::*;
use plakous_hosted::host_service::AttachmentCount;
mod linked;
mod loss;

fn fixture() -> (tempfile::TempDir, Arc<TerminalRegistry>, AttachmentCount) {
    let directory = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(directory.path().join("home"));
    paths.ensure_layout().unwrap();
    let registry = TerminalRegistry::new(paths, 7).unwrap();
    let count = AttachmentCount::default();
    let holds = count.clone();
    registry
        .bind_residency(Arc::new(move |_| {
            holds.hold().ok_or_else(|| "releasing".into())
        }))
        .unwrap();
    (directory, registry, count)
}

fn context(login: &str) -> HostRequestContext {
    HostRequestContext {
        principal: HostPrincipal::Web(login.into()),
        connection_id: 1,
        host_generation: 7,
        request_id: 1,
    }
}

fn pin(registry: &TerminalRegistry, workspace: PathBuf) -> TerminalPin {
    TerminalPin {
        host_generation: 7,
        host_id: registry.host_id.clone(),
        generation: registry.runner_generation.clone(),
        namespace: registry.namespace(&context("operator").principal).unwrap(),
        runner: "local".into(),
        instance: "instance".into(),
        key: TerminalKey {
            session: id("session".into()).unwrap(),
            node: id("node".into()).unwrap(),
            terminal_id: id("terminal".into()).unwrap(),
        },
        workspace,
        digest: "digest".into(),
    }
}

#[test]
fn operator_host_memory_pin_keeps_residency_and_reclamation_until_proof() {
    let (directory, registry, count) = fixture();
    let identity = registry
        .reserve(
            pin(&registry, directory.path().to_owned()),
            count.hold().unwrap(),
        )
        .unwrap();
    assert_eq!(count.get(), 1);
    assert_eq!(
        registry.resident_reason("session"),
        Some(ResidentReason::OperatorTerminal)
    );
    assert!(registry.admit_local("local", directory.path()).is_err());
    assert!(!registry.paths.product_database().exists());
    registry.release_proven(&identity).unwrap();
    assert_eq!(count.get(), 0);
    assert!(registry.admit_local("local", directory.path()).is_ok());
    assert!(!registry.paths.product_database().exists());
    assert_eq!(crate::store::SCHEMA_VERSION, 37);
    assert!(crate::home::state::state_key().contains(".p37."));
}

#[test]
fn operator_host_listing_bounds_completed_receipts_without_forgetting_open_fences() {
    let (directory, registry, count) = fixture();
    for n in 0..13 {
        let mut terminal = pin(&registry, directory.path().to_owned());
        terminal.key.terminal_id = id(format!("complete-{n:02}")).unwrap();
        let identity = registry.reserve(terminal, count.hold().unwrap()).unwrap();
        registry.release_proven(&identity).unwrap();
    }
    assert_eq!(registry.list(&context("operator")).unwrap().len(), 8);
    assert_eq!(registry.lock().entries.len(), 13);
    assert_eq!(registry.list(&context("other")).unwrap().len(), 0);
    assert_eq!(count.get(), 0);
    assert!(!registry.paths.product_database().exists());
}

#[test]
fn operator_host_reclaim_admission_blocks_new_pins_without_persisting_them() {
    let (directory, registry, count) = fixture();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let child = workspace.join("child");
    std::fs::create_dir(&child).unwrap();
    let admission = registry.admit_local("local", &workspace).unwrap();
    assert!(matches!(
        registry.reserve(pin(&registry, child.clone()), count.hold().unwrap()),
        Err(TerminalError::Busy)
    ));
    assert_eq!(count.get(), 0);
    drop(admission);
    registry
        .reserve(pin(&registry, child.clone()), count.hold().unwrap())
        .unwrap();
    let paths = registry.paths.clone();
    drop(registry);
    let restarted = TerminalRegistry::new(paths, 8).unwrap();
    assert!(!restarted.blocks_idle_exit());
    assert!(
        restarted
            .list(&HostRequestContext {
                host_generation: 8,
                ..context("operator")
            })
            .unwrap()
            .is_empty()
    );
    assert!(
        restarted.binding("local").is_err(),
        "first bind must discover runner-owned pins"
    );
    assert!(!restarted.paths.product_database().exists());
}

#[tokio::test]
async fn operator_host_quiet_reclamation_holds_existing_startup_lock_without_store_or_daemon() {
    let (directory, registry, _) = fixture();
    let fence = RunnerFence::new(registry.paths.clone(), None);
    let admission = fence.admit("local", directory.path()).await.unwrap();
    let socket = registry
        .paths
        .home()
        .join("host/runner")
        .join(format!("v{}", crate::commands::runner::version().protocol))
        .join("run/runner.sock");
    assert!(!socket.exists(), "reclamation never auto-starts a daemon");
    let probe = std::fs::OpenOptions::new()
        .write(true)
        .open(socket.with_extension("lock"))
        .unwrap();
    assert!(matches!(
        probe.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(admission);
    probe.try_lock().unwrap();
    assert!(!registry.paths.product_database().exists());
}
