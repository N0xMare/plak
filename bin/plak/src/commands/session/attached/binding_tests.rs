use super::*;

#[test]
fn absent_model_is_unresolvable_only_in_a_ready_catalog() {
    for ready in [false, true] {
        let catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
            plakous_host_protocol::views::ProviderSection {
                account: "claude".into(),
                label: "Claude".into(),
                ready,
                entries: Vec::new(),
            },
        ]);
        let error = require_catalog_binding(
            &product_config::ProductTargetSelection::Native,
            &catalog,
            &[],
            "claude",
            "removed-model",
        )
        .unwrap_err();
        assert_eq!(matches!(error, AttachedConfigError::Unresolvable(_)), ready);
        let cause = CliFailure::from(error);
        assert!(cause.message().contains(if ready {
            "no longer in the Ready"
        } else {
            "not Ready"
        }));
    }
}

#[test]
fn picker_filters_are_not_evidence_that_the_catalog_removed_a_model() {
    let catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
        plakous_host_protocol::views::ProviderSection {
            account: "codex".into(),
            label: "Codex".into(),
            ready: true,
            entries: Vec::new(),
        },
    ]);
    let facts = [crate::presentation::TuiNativeModelFact {
        account: "codex".into(),
        exact_id: "filtered-model".into(),
        native_tool_mode: None,
        reasoning_efforts: Vec::new(),
        service_tiers: Vec::new(),
    }];
    require_catalog_binding(
        &product_config::ProductTargetSelection::Native,
        &catalog,
        &facts,
        "codex",
        "filtered-model",
    )
    .unwrap();
    assert!(
        !crate::commands::host_config::attached_target_accepts_effort(
            &product_config::ProductTargetSelection::Native,
            &catalog,
            "codex",
            "filtered-model",
            true
        )
    );
}

#[test]
fn exact_actions_and_carry_probes_do_not_require_a_ready_picker() {
    let catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
        plakous_host_protocol::views::ProviderSection {
            account: "codex".into(),
            label: "Codex".into(),
            ready: false,
            entries: Vec::new(),
        },
    ]);
    validate_catalog_for_attach(
        &product_config::ProductTargetSelection::Native,
        &catalog,
        &[],
        "codex",
        "known-model",
        true,
        false,
    )
    .unwrap();
    let error = validate_catalog_for_attach(
        &product_config::ProductTargetSelection::Native,
        &catalog,
        &[],
        "codex",
        "known-model",
        true,
        true,
    )
    .unwrap_err();
    assert!(CliFailure::from(error).message().contains("before /rebind"));
}
