use super::*;

#[test]
fn python_compositions_share_the_worker_gate_but_keep_selected_refusal_ids() {
    use crate::product_config::ProductCompositionSelection;
    for composition in [
        ProductCompositionSelection::PyPtc,
        ProductCompositionSelection::PySdk,
    ] {
        assert!(composition.requires_python_worker());
        if let Err(error) = require_composition(composition) {
            assert_eq!(error.id, composition.as_str());
            assert_eq!(error.feature, "python");
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("`{}` is not included", composition.as_str()))
            );
        }
    }
    let snapshot = crate::shell::settings::settings_view(
        &crate::product_config::ProductConfig::default_product(),
    );
    for option in snapshot.product.composition_options {
        let composition: ProductCompositionSelection = option.composition.into();
        assert!(require_composition(composition).is_ok());
    }
}

#[test]
fn image_argument_refusal_preserves_the_capability_source_before_file_io() {
    let failure = crate::cli::parse_with_image_capability(
        [
            "run",
            "--account",
            "grok",
            "--model",
            "m",
            "--image",
            "/nonexistent-image.png",
            "--",
            "describe",
        ]
        .map(std::ffi::OsString::from),
        Err(plakous_hosted::capabilities::CapabilityUnavailable {
            id: "images",
            kind: plakous_hosted::capabilities::CapabilityKind::Surface,
            feature: "images",
        }),
    )
    .unwrap_err();
    assert_eq!(failure.exit_code(), 2);
    let source = std::error::Error::source(&failure).unwrap();
    let capability = source
        .downcast_ref::<plakous_hosted::capabilities::CapabilityUnavailable>()
        .expect("capability refusal remains typed");
    assert_eq!(capability.feature, "images");
}

#[test]
fn session_admission_ignores_unused_python_and_mcp_templates() {
    use crate::product_config::{AgentTemplate, ProductCompositionSelection, ProductConfig};
    let mut config = ProductConfig::default_product();
    config.root_agent = Some("plain".to_owned());
    let plain = AgentTemplate {
        account: "openai".to_owned(),
        model: "gpt-6-astra".to_owned(),
        composition: Some(ProductCompositionSelection::Direct),
        ..AgentTemplate::default()
    };
    config.templates.insert("plain".to_owned(), plain);
    let unused = AgentTemplate {
        account: "codex".to_owned(),
        model: "gpt-6-astra".to_owned(),
        composition: Some(ProductCompositionSelection::PyPtc),
        tools: plakous_hosted::tool_plan::TemplateToolSelection::parse(&["mcp:unused"]).unwrap(),
        ..AgentTemplate::default()
    };
    config.templates.insert("unused".to_owned(), unused);
    admit_selected(
        &config,
        Some("openai"),
        Some("gpt-6-astra"),
        ProductCompositionSelection::Direct,
    )
    .unwrap();
    if require_feature("python").is_err() || require_feature("mcp").is_err() {
        assert!(
            admit_config(&config).is_err(),
            "doctor still scans unused templates"
        );
    }
}

#[test]
fn capability_inventory_contains_canonical_routes_and_product_features_only() {
    let expected: std::collections::BTreeSet<_> = [
        "anthropic",
        "claude",
        "codex",
        "grok",
        "openai",
        "openrouter",
        "xai-api",
    ]
    .into_iter()
    .filter(|account| {
        plakous_hosted::capabilities::for_account(account)
            .unwrap()
            .compiled
    })
    .chain(
        plakous_hosted::capabilities::BUILD_CAPABILITIES
            .iter()
            .filter(|entry| entry.compiled)
            .map(|entry| entry.feature),
    )
    .collect();
    let actual = compiled_features();
    assert_eq!(
        actual
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    assert_eq!(actual.len(), expected.len());
}

#[test]
fn registry_refusals_keep_the_feature_identity_and_usage_exit() {
    for entry in plakous_hosted::capabilities::BUILD_CAPABILITIES {
        match require_feature(entry.feature) {
            Ok(()) => assert!(entry.compiled),
            Err(error) => {
                assert!(!entry.compiled);
                assert_eq!(
                    (error.id, error.kind, error.feature),
                    (entry.id, entry.kind, entry.feature)
                );
                let failure = crate::CliFailure::from(error);
                assert_eq!(failure.exit_code(), 2);
                assert!(failure.message().contains("available:"));
                assert!(std::error::Error::source(&failure).is_some());
            }
        }
    }
}

#[test]
fn default_version_stays_exact_and_omissions_are_sorted() {
    let omissions = omitted_features();
    assert!(omissions.windows(2).all(|pair| pair[0] < pair[1]));
    if omissions.is_empty() {
        assert_eq!(version_suffix(), "");
        assert_eq!(description(), "default");
    } else {
        assert!(version_suffix().starts_with("; features -"));
        for feature in omissions {
            assert!(version_suffix().contains(&format!("-{feature}")));
        }
    }
    let features = compiled_features();
    assert!(features.windows(2).all(|pair| pair[0] < pair[1]));
}
