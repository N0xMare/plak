use crate::{CliResult, command::Command};
pub(crate) use plakous_hosted::capabilities::{
    compiled_features, omitted_features, require_feature,
};

pub(crate) fn admit_command(command: &Command) -> CliResult<()> {
    match command {
        Command::Tui { .. } | Command::Resume { .. } => require_feature("tui")?,
        Command::Mcp(_) => require_feature("mcp")?,
        Command::Run { composition, .. }
            if composition == &Some(crate::product_config::ProductCompositionSelection::PyPtc) =>
        {
            require_feature("python")?;
        }
        Command::RunOpenRouter { composition, .. } | Command::RunPlatform { composition, .. }
            if *composition == crate::product_config::ProductCompositionSelection::PyPtc =>
        {
            require_feature("python")?;
        }
        _ => {}
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
    for template in config.templates.values() {
        if template.composition == Some(crate::product_config::ProductCompositionSelection::PyPtc) {
            require_feature("python")?;
        }
        if template.tools.mcp_servers().next().is_some() {
            require_feature("mcp")?;
        }
    }
    Ok(())
}

pub(crate) fn admit_selected(
    config: &crate::product_config::ProductConfig,
    account: Option<&str>,
    model: Option<&str>,
    composition: crate::product_config::ProductCompositionSelection,
) -> CliResult<()> {
    if composition == crate::product_config::ProductCompositionSelection::PyPtc {
        require_feature("python")?;
    }
    if let Some(template) = config.root_template()
        && Some(template.account.as_str()) == account
        && Some(template.model.as_str()) == model
        && template.tools.mcp_servers().next().is_some()
    {
        require_feature("mcp")?;
    }
    Ok(())
}

pub(crate) fn admit_session(record: &crate::store::sessions::SessionRecord) -> CliResult<()> {
    if record.composition == crate::product_config::ProductCompositionSelection::PyPtc {
        require_feature("python")?;
    }
    if record
        .frozen_tools
        .as_ref()
        .is_some_and(|tools| tools.selection().selection.mcp_servers().next().is_some())
    {
        require_feature("mcp")?;
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
