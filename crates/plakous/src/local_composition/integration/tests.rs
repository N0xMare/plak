use super::*;

#[test]
fn filtered_native_catalog_rebuild_keeps_file_writing_absent() {
    let mut builder = plakous_tools::ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory().expect("packs"),
        &mut builder,
    )
    .expect("register");
    CodexMinimalLocalCodeModeV1::register_markers_into(&mut builder).expect("markers");
    let registry = builder.freeze();
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "openai.codex.responses",
        "openai.codex_subscription",
        "openai.codex.compat.0.153.3",
        "responses-family",
        "gpt-5.3-codex",
        "gpt-5.3-codex",
        serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(
                    DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[b"filtered-native"],
                )
                .expect("digest"),
            )
            .expect("json"),
        )
        .expect("configuration"),
        1,
    )
    .expect("target");
    let catalog = ["exec_command", "write_stdin", "update_plan"]
        .into_iter()
        .map(|name| ToolName::new(name).expect("name"))
        .collect::<Vec<_>>();
    let original = CodexMinimalLocalCodeModeV1::compile_for_catalog(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target.clone(),
        &catalog,
    )
    .expect("descriptor");
    let integration = CellIntegration {
        worker: "unused".into(),
        identity: Vec::new(),
        presentation: Box::new(JavaScriptPresentation::CodexMinimal {
            nested_catalog: catalog,
        }),
    };
    let rebuilt = integration
        .rebuild_descriptor(plakous_tools::JobConfig::default(), registry, target)
        .expect("rebuild");
    assert_eq!(
        rebuilt.identity,
        neutral_minimal_descriptor(&original)
            .expect("neutral")
            .identity
    );
    assert!(
        rebuilt
            .nested_projection
            .by_exposed(&ToolName::new("apply_patch").expect("name"))
            .is_none()
    );
}

#[test]
fn code_mode_wait_clamps_to_ten_minutes_and_keeps_its_default() {
    let integration = CellIntegration {
        worker: "unused-test-worker".into(),
        identity: Vec::new(),
        presentation: Box::new(JavaScriptPresentation::CodexExact),
    };
    assert_eq!(
        integration
            .parse_wait(serde_json::json!({"cell_id": "1"}))
            .expect("default wait")
            .yield_time_ms,
        10_000
    );
    for (hint, expected) in [
        (0, 0),
        (10_001, 10_001),
        (600_000, 600_000),
        (900_000, 600_000),
    ] {
        assert_eq!(
            integration
                .parse_wait(serde_json::json!({"cell_id": "1", "yield_time_ms": hint}))
                .expect("whole wait hint")
                .yield_time_ms,
            expected
        );
    }
    for hint in [
        serde_json::json!(-1),
        serde_json::json!(600_000.5),
        serde_json::json!("900000"),
    ] {
        assert!(
            integration
                .parse_wait(serde_json::json!({"cell_id": "1", "yield_time_ms": hint}))
                .is_err()
        );
    }
}
