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
fn operator_host_write_ahead_pin_keeps_residency_and_reclamation_until_proof() {
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
    assert!(registry.admit("local", directory.path()).is_err());
    assert_eq!(registry.store().unwrap().terminal_pins().unwrap().len(), 1);
    assert!(
        registry
            .store()
            .unwrap()
            .terminal_session_pinned("session")
            .unwrap()
    );
    assert!(
        registry
            .store()
            .unwrap()
            .delete_session("session", 0)
            .unwrap_err()
            .contains("operator_terminal")
    );
    registry.release_proven(&identity).unwrap();
    assert_eq!(count.get(), 0);
    assert!(registry.admit("local", directory.path()).is_ok());
    assert_eq!(registry.store().unwrap().terminal_pins().unwrap().len(), 0);
    assert!(
        !registry
            .store()
            .unwrap()
            .terminal_session_pinned("session")
            .unwrap()
    );
}

#[test]
fn operator_host_reclaim_admission_blocks_new_pins_and_restart_retains_old_pins() {
    let (directory, registry, count) = fixture();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let child = workspace.join("child");
    std::fs::create_dir(&child).unwrap();
    let admission = registry.admit("local", &workspace).unwrap();
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
    let restarted = TerminalRegistry::new(paths.clone(), 8).unwrap();
    assert!(restarted.blocks_idle_exit());
    assert!(restarted.admit("local", &workspace).is_err());
    assert!(
        crate::store::terminal_pins::StoredTerminalFence(paths)
            .admit("local", &child.join("target"))
            .is_err()
    );
    let restored = AttachmentCount::default();
    restarted.retain_holds("session", &restored).unwrap();
    assert_eq!(restored.get(), 1);
    assert_eq!(
        restarted.resident_reason("session"),
        Some(ResidentReason::OperatorTerminal)
    );
}
