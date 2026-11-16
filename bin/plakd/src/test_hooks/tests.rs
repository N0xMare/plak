use super::profile_ready_catalog;
use plakous_host_protocol::views::{ModelEntry, ProviderSection, ReadyCatalog};

#[test]
fn profile_efforts_change_configuration_without_changing_route_or_account()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use plakous::model::ReasoningEffort;
    use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
    let workspace = tempfile::tempdir()?;
    let controller = BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        workspace.path().join("profile-bindings.jsonl"),
    )?;
    let binding = |effort| -> Result<_, Box<dyn std::error::Error + Send + Sync>> {
        let mut plan: plakous_offline_crash_harness::script::ScriptPlan =
            serde_json::from_value(serde_json::json!({"version": 1}))?;
        plan.profile_effort = Some(effort);
        Ok(scripted_runtime_binding(
            workspace.path(),
            controller.clone(),
            std::sync::Arc::new(plan),
        )?
        .target_binding())
    };
    let medium = binding(ReasoningEffort::Medium)?;
    let low = binding(ReasoningEffort::Low)?;
    assert_eq!(medium.protocol(), low.protocol());
    assert_eq!(medium.route(), low.route());
    assert_eq!(medium.model(), low.model());
    assert_eq!(medium.account_binding(), low.account_binding());
    assert_ne!(
        medium.selected_provider_configuration(),
        low.selected_provider_configuration()
    );
    assert_ne!(
        medium.effective_provider_configuration(),
        low.effective_provider_configuration()
    );
    Ok(())
}

#[test]
fn offline_profiles_preserve_real_route_readiness() {
    for ready in [false, true] {
        let real = ModelEntry {
            id: "real-model".to_owned(),
            tag: "real catalog".to_owned(),
            reasoning: false,
            effort_options: Vec::new(),
            extras: Vec::new(),
            unavailable_reason: None,
        };
        let original = ReadyCatalog::new(vec![
            ProviderSection {
                account: "codex".to_owned(),
                label: "Codex".to_owned(),
                ready,
                entries: vec![real.clone()],
            },
            ProviderSection {
                account: "claude".to_owned(),
                label: "Claude".to_owned(),
                ready: !ready,
                entries: vec![real.clone()],
            },
        ]);
        let injected = profile_ready_catalog(&original);
        assert_eq!(injected.accounts.len(), original.accounts.len());
        assert_eq!(injected.accounts[1], original.accounts[1]);
        assert_eq!(original.accounts[0].ready, ready);
        let codex = &injected.accounts[0];
        assert!(codex.ready);
        assert_eq!(codex.entries.contains(&real), ready);
        assert!(codex.entries.iter().any(|row| row.id == super::SELECTOR));
        assert_eq!(profile_ready_catalog(&injected), injected);
    }
}
