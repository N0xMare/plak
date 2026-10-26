use crate::{CliResult, command::Command};
pub(crate) use plakous_hosted::capabilities::{
    compiled_features, omitted_features, require_composition, require_feature,
};

pub(crate) fn require_account(account: &str) -> CliResult<()> {
    plakous_hosted::capabilities::require(account)?;
    Ok(())
}

pub(crate) fn admit_command(command: &Command) -> CliResult<()> {
    match command {
        Command::Tui { .. } | Command::Resume { .. } => require_feature("tui")?,
        Command::Mcp(_) => require_feature("mcp")?,
        Command::Run {
            composition: Some(composition),
            ..
        } => {
            require_composition(*composition)?;
        }
        Command::RunOpenRouter { composition, .. } | Command::RunPlatform { composition, .. } => {
            require_composition(*composition)?;
        }
        _ => {}
    }
    match command {
        Command::CompatibilityNotice(provider)
        | Command::AuthStatus(provider)
        | Command::AuthRefresh(provider)
        | Command::Models(provider)
        | Command::AuthLogin { provider, .. }
        | Command::AuthLogout { provider, .. }
        | Command::Run { provider, .. } => plakous_hosted::capabilities::require_native(*provider)?,
        Command::ModelsOpenRouter | Command::RunOpenRouter { .. } => {
            plakous_hosted::capabilities::require("openrouter")?;
        }
        Command::ModelsPlatform(provider) | Command::RunPlatform { provider, .. } => {
            plakous_hosted::capabilities::require(provider.account())?;
        }
        _ => {}
    }
    Ok(())
}

// Refuse before parsing reads a file.
pub(crate) fn admit_image_arguments(
    images: &[std::path::PathBuf],
    images_available: Result<(), plakous_hosted::capabilities::CapabilityUnavailable>,
) -> CliResult<()> {
    if !images.is_empty() {
        images_available?;
    }
    Ok(())
}

pub(crate) fn description() -> String {
    let omitted = omitted_features();
    if omitted.is_empty() {
        "default".to_owned()
    } else {
        format!("omitted {}", omitted.join(", "))
    }
}

pub(crate) fn admit_config(config: &crate::product_config::ProductConfig) -> CliResult<()> {
    require_composition(config.default_composition())?;
    for template in config.templates.values() {
        require_account(&template.account)?;
        if let Some(composition) = template.composition {
            require_composition(composition)?;
        }
        plakous_hosted::capabilities::require_tools(&template.tools)?;
    }
    Ok(())
}

pub(crate) fn admit_selected(
    config: &crate::product_config::ProductConfig,
    account: Option<&str>,
    model: Option<&str>,
    composition: crate::product_config::ProductCompositionSelection,
) -> CliResult<()> {
    if let Some(account) = account {
        require_account(account)?;
    }
    require_composition(composition)?;
    if let Some(template) = config.root_template()
        && Some(template.account.as_str()) == account
        && Some(template.model.as_str()) == model
    {
        plakous_hosted::capabilities::require_tools(&template.tools)?;
    }
    Ok(())
}

pub(crate) fn admit_session(record: &crate::store::sessions::SessionRecord) -> CliResult<()> {
    if let Some(account) = record.account.as_deref() {
        require_account(account)?;
    }
    require_composition(record.composition)?;
    if let Some(tools) = record.frozen_tools.as_ref() {
        plakous_hosted::capabilities::require_tools(&tools.selection().selection)?;
    }
    Ok(())
}

pub(crate) fn version_suffix() -> String {
    let omitted = omitted_features();
    if omitted.is_empty() {
        String::new()
    } else {
        format!(
            "; features {}",
            omitted
                .iter()
                .map(|name| format!("-{name}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

#[cfg(test)]
mod tests {
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
        let snapshot = crate::shell::settings::settings_snapshot(
            &crate::product_config::ProductConfig::default_product(),
        );
        for option in snapshot.composition_options {
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
            tools: plakous_hosted::tool_plan::TemplateToolSelection::parse(&["mcp:unused"])
                .unwrap(),
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
}
