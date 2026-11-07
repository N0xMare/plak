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
        Command::Tui { .. }
        | Command::TuiPlacement { .. }
        | Command::Resume { .. }
        | Command::HostResume { .. } => {
            require_feature("tui")?;
        }
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
        Command::CompatibilityNotice(provider) => {
            plakous_hosted::capabilities::require_native(*provider)?;
        }
        Command::AuthStatus(provider)
        | Command::AuthRefresh(provider)
        | Command::Models(provider)
        | Command::AuthLogin { provider, .. }
        | Command::AuthLogout { provider, .. }
        | Command::Run { provider, .. } => {
            plakous_hosted::capabilities::require(provider.as_str())?;
        }
        Command::ModelsOpenRouter(account) | Command::RunOpenRouter { account, .. } => {
            plakous_hosted::capabilities::require(account.as_str())?;
        }
        Command::ModelsPlatform(account) | Command::RunPlatform { account, .. } => {
            plakous_hosted::capabilities::require(account.as_str())?;
        }
        Command::Agent {
            account: Some(account),
            ..
        } => {
            plakous_hosted::capabilities::require(account.as_str())?;
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
        record
            .target_selection
            .validate_account(&account.parse()?)?;
    } else if record.model.is_some() || record.target_selection.is_api() {
        return Err(
            plakous_hosted::capabilities::AccountAdmissionError::AccountTargetMismatch {
                account: "<unbound>".into(),
            }
            .into(),
        );
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
mod tests;
