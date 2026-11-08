//! The binding a new session is seeded with.

use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult};
use plakous_hosted::sessions::TranscriptRow;

/// Seeds a new session from the root template. A direct API account binds the
/// composition `platform::api_composition` decides for its model, as
/// `plak run` does; with no model the composition stays unresolved.
pub fn seed_new_session_binding(
    record: &mut product_sessions::SessionRecord,
    config: &product_config::ProductConfig,
) -> CliResult<()> {
    seed_new_session_binding_on(record, config, &plakous::tools::LocalWorkspaceProbe)
}

/// Seed a binding using the workspace owner's probe, never host path guesses.
pub(crate) fn seed_new_session_binding_on(
    record: &mut product_sessions::SessionRecord,
    config: &product_config::ProductConfig,
    probe: &dyn plakous::tools::WorkspaceProbe,
) -> CliResult<()> {
    let exists = |path: &std::path::Path| {
        probe
            .canonicalize(path)
            .map(|path| path.is_some())
            .map_err(|error| CliFailure::new(error.to_string()))
    };
    if !crate::build_identity::JJ_ENABLED && exists(&record.workspace_root.join(".jj"))? {
        if !exists(&record.workspace_root.join(".git"))? {
            return Err(CliFailure::new(
                "this build has no jj support; use a build with the jj feature",
            ));
        }
        record.record_notice(TranscriptRow::info(
            product_sessions::NoticeScope::Session,
            "This build has no jj support; using git copies in this colocated repository.",
        ));
    }
    record.account = config.default_account().map(str::to_owned);
    record.model = config.default_model().map(str::to_owned);
    validate_new_session_binding(record)?;
    if record.account.as_deref() == Some("openrouter") {
        record.effort = None;
    } else {
        record.effort = config.default_effort().map(str::to_owned);
    }
    record.composition =
        config.composition_for_tuple(record.account.as_deref(), record.model.as_deref());
    if record.account.as_deref() == Some("openrouter")
        && record.composition == product_config::ProductCompositionSelection::FollowCatalog
    {
        record.composition = product_config::ProductCompositionSelection::Direct;
    }
    if let Some(provider) = record
        .account
        .as_deref()
        .and_then(plakous_hosted::binding::PlatformProvider::for_account)
    {
        record.target_selection = product_config::ProductTargetSelection::Platform { provider };
        resolve_api_composition(record, provider)?;
    } else if record.account.as_deref() == Some("codex")
        && config.default_extra() == Some("priority")
    {
        record.target_selection = product_config::ProductTargetSelection::NativePriority;
    } else if record.account.as_deref() == Some("openrouter") {
        record.target_selection = product_config::ProductTargetSelection::OpenRouter {
            wire: product_config::ProductWireBinding::resolve(config.default_protocol()),
            routing: product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
        };
    }
    Ok(())
}

/// Binds the composition the product's API table admits for the record's
/// model; an inadmissible selection is refused, never substituted.
pub(crate) fn resolve_api_composition(
    record: &mut product_sessions::SessionRecord,
    provider: plakous_hosted::binding::PlatformProvider,
) -> CliResult<()> {
    let Some(model) = record.model.as_deref() else {
        return Ok(());
    };
    record.composition =
        plakous_hosted::platform::api_composition(provider, model, record.composition).map_err(
            |error| {
                CliFailure::classified(
                    CliFailureKind::Usage,
                    format!("{}: {error}", provider.account()),
                )
            },
        )?;
    Ok(())
}

fn validate_new_session_binding(record: &product_sessions::SessionRecord) -> CliResult<()> {
    if let (Some(account), Some(model)) = (record.account.as_deref(), record.model.as_deref()) {
        crate::commands::support::admit_account_model(account, model).map_err(|error| {
            CliFailure::classified(
                CliFailureKind::Usage,
                format!("invalid root binding: {error}; {}", ROOT_BINDING_REMEDY),
            )
        })?;
    }
    Ok(())
}

/// A construction refusal never grants permission to edit the user's defaults.
pub(crate) const ROOT_BINDING_REMEDY: &str = "select a valid root template with `plak config set root.agent <template>`; config.toml was not changed";

#[cfg(test)]
mod vcs_selection_tests {
    use super::*;

    #[test]
    fn a_placed_session_uses_its_runner_probe_not_the_same_spelled_host_directory() {
        struct Remote;
        impl plakous::tools::WorkspaceProbe for Remote {
            fn canonicalize(
                &self,
                _: &std::path::Path,
            ) -> Result<Option<std::path::PathBuf>, plakous::tools::ProbeError> {
                Ok(None)
            }
            fn is_file(&self, _: &std::path::Path) -> Result<bool, plakous::tools::ProbeError> {
                panic!("not a binding probe")
            }
            fn tree_bytes(
                &self,
                _: &std::path::Path,
            ) -> Result<Option<u64>, plakous::tools::ProbeError> {
                panic!("not a binding probe")
            }
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".jj")).unwrap();
        let mut record = product_sessions::SessionRecord::new(root.path().to_owned());
        let config = product_config::ProductConfig::default_product();
        seed_new_session_binding_on(&mut record, &config, &Remote).unwrap();
        assert_eq!(record.transcript.pending_notices(), []);
    }

    #[test]
    fn new_session_refuses_jj_only_or_records_one_colocated_notice() {
        if crate::build_identity::JJ_ENABLED {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".jj")).unwrap();
        let config = product_config::ProductConfig::default_product();
        let mut record = product_sessions::SessionRecord::new(root.path().to_path_buf());
        let error = seed_new_session_binding(&mut record, &config).unwrap_err();
        assert!(error.to_string().contains("jj feature"));
        std::fs::create_dir(root.path().join(".git")).unwrap();
        seed_new_session_binding(&mut record, &config).unwrap();
        assert_eq!(record.transcript.pending_notices().len(), 1);
    }
}
