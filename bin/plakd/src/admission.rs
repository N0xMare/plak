use crate::CliResult;

pub(crate) fn admit_config(config: &crate::product_config::ProductConfig) -> CliResult<()> {
    for template in config.templates.values() {
        plakous_hosted::capabilities::require(&template.account)?;
    }
    Ok(())
}

pub(crate) fn admit_session(record: &crate::store::sessions::SessionRecord) -> CliResult<()> {
    if let Some(account) = record.account.as_deref() {
        plakous_hosted::capabilities::require(account)?;
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
    Ok(())
}

#[cfg(test)]
mod tests;
