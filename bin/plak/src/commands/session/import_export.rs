//! Export and import a session.

use super::lookup::load_exact_product_session;
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::runtime as product_host;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// `plak session export <id> [--out <path>]`: writes the session's portable
/// document, read-only, and prints where.
pub(crate) fn export_session(
    services: &crate::shell::services::Services,
    id: &str,
    out: Option<PathBuf>,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let paths = services.paths()?;
    let record = load_exact_product_session(paths, services.session_records()?.as_ref(), id)?;
    let document = crate::carry::export_session(paths, &record).map_err(|refusal| {
        CliFailure::new(format!(
            "session {} cannot be exported: {refusal}",
            record.id
        ))
    })?;
    let path = match out {
        Some(path) => path,
        None => crate::carry::new_carry_file(paths, &record.id).map_err(CliFailure::new)?,
    };
    crate::carry::write_document(&path, &document).map_err(CliFailure::new)?;
    writeln!(stdout, "{}", path.display()).map_err(|_| CliFailure::new("stdout failed"))
}

/// Creates a new session from the export document at `path` and returns its
/// record and the note that says where it came from. The document is left in
/// place; the caller decides whether it is spent. A document whose session
/// was already imported is refused, naming the session it became.
pub(crate) fn import_session(
    services: &crate::shell::services::Services,
    path: &Path,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    let document =
        crate::carry::read_document(path).map_err(|error| CliFailure::new(error.to_string()))?;
    import_document(services, document, false)
}

pub(crate) struct PreparedImport {
    pub(super) carried: crate::carry::CarriedImport,
    pub(super) estimated_tokens: u64,
    pub(super) messages: usize,
}

impl PreparedImport {
    pub(crate) fn new(
        document: crate::carry::CarryDocument,
        local_handoff: bool,
    ) -> CliResult<Self> {
        let estimated_tokens = document.estimated_tokens();
        let messages = document.message_count();
        let carried = document
            .into_import_with_replay(local_handoff)
            .map_err(|e| CliFailure::new(e.to_string()))?;
        crate::build_features::admit_session(&carried.record)?;
        Ok(Self {
            carried,
            estimated_tokens,
            messages,
        })
    }
}

pub(crate) fn import_document(
    services: &crate::shell::services::Services,
    document: crate::carry::CarryDocument,
    local_handoff: bool,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    import_prepared(services, PreparedImport::new(document, local_handoff)?)
}

pub(crate) fn import_prepared(
    services: &crate::shell::services::Services,
    prepared: PreparedImport,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    import_prepared_with_fallback(services, prepared, None)
}

pub(super) fn import_prepared_with_fallback(
    services: &crate::shell::services::Services,
    prepared: PreparedImport,
    root_fallback: Option<&product_config::ProductConfig>,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    let PreparedImport {
        carried,
        estimated_tokens,
        messages,
    } = prepared;
    let crate::carry::CarriedImport {
        record,
        mut history,
        mut note,
    } = carried;
    let paths = crate::home::paths::PlakPaths::for_workspace(
        services.paths()?.home().to_path_buf(),
        &record.workspace_root,
    );
    product_config::SessionPolicy::load(&paths).map_err(CliFailure::new)?;
    let (source_id, source_key) = record
        .carried_forward_from()
        .ok_or_else(|| CliFailure::new("missing carry lineage"))?;
    let (source_id, source_key) = (source_id.to_owned(), source_key.to_owned());
    if let Some(existing) = crate::carry::already_carried(
        services.session_records()?.as_ref(),
        &record.workspace_root,
        &source_id,
        &source_key,
    )
    .map_err(CliFailure::new)?
    {
        return Err(CliFailure::new(format!(
            "session {source_id} ({source_key}) was already carried forward as {}; resume it with `plak --resume {}`",
            existing.id, existing.id
        )));
    }
    record.validate_for_store().map_err(CliFailure::new)?;
    let mut config = import_host_config(services, record, root_fallback, &mut history, &mut note)?;
    config.seed_history = Some(history);
    let created = product_host::create_seeded_session_with_preflight(config, move |window| {
        let capacity =
            super::attached::carry_capacity_from_window(window, estimated_tokens, messages);
        let message = capacity.warning().unwrap_or_else(|| {
            if capacity.admitted_input.is_none() {
                "destination window unknown; carry will be checked at first prompt".to_owned()
            } else {
                String::new()
            }
        });
        if !message.is_empty() {
            writeln!(
                io::stderr(),
                "warning: {}",
                crate::cli_output::safe_terminal_block(&message)
            )
            .map_err(|_| CliFailure::new("stderr failed"))?;
        }
        Ok(())
    })?;
    if let Some(error) = &created.completion_error {
        note.push_str(&format!(
            "; session {} was created but completion failed: {}; resuming the committed successor {}",
            created.session.id, error.message(), created.session.id
        ));
    }
    let (record, notes) = created.into_session();
    let projected = (|| -> CliResult<()> {
        let executor = plakous::application::ApplicationExecutor::new()
            .map_err(|_| CliFailure::new("application executor failed"))?;
        let sessions = services.state_roots()?.runtime_sessions()?;
        let records = services.session_records()?;
        executor
            .block_on(product_host::project_session(&sessions, &records, &record))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
        Ok(())
    })();
    if projected.is_err() {
        note.push_str(&format!(
            "; warning: session {} was imported but its views need a projector pass; open it once",
            record.id
        ));
    }
    if !notes.trim().is_empty() {
        let _ = io::Write::write_all(
            &mut io::stderr(),
            format!("{}\n", notes.trim_end()).as_bytes(),
        );
    }
    Ok((record, note))
}

fn import_host_config(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
    root_fallback: Option<&product_config::ProductConfig>,
    history: &mut plakous::sessions::PortableSessionHistory,
    note: &mut String,
) -> CliResult<product_host::HostConfig<crate::surface_protocol::PlakExtension>> {
    let source = if root_fallback.is_some() {
        super::attached::rebind_host_config(services, record.clone())
    } else {
        super::attached::attached_host_config(services, record.clone())
    };
    let source_error = match source {
        Ok(config) => return Ok(config),
        Err(super::attached::AttachedConfigError::Unresolvable(error)) => error,
        Err(super::attached::AttachedConfigError::Other(error)) => return Err(error),
    };
    let Some(root) = root_fallback else {
        return Err(source_error);
    };
    let mut fallback = record.clone();
    fallback.target_selection = product_config::ProductTargetSelection::Native;
    super::binding::seed_new_session_binding(&mut fallback, root)?;
    if fallback.account != record.account
        || fallback.model != record.model
        || fallback.effort != record.effort
        || fallback.composition != record.composition
        || fallback.target_selection != record.target_selection
    {
        *history = history
            .clone()
            .without_replay()
            .map_err(|error| CliFailure::new(error.to_string()))?;
    }
    let notice = format!(
        "source binding could not be resolved ({}); using the configured root binding",
        plakous::model::SafeDiagnostic::new(source_error.message())
    );
    fallback.record_notice(plakous_hosted::sessions::TranscriptRow::info(
        product_sessions::NoticeScope::Session,
        &notice,
    ));
    note.push_str(&format!("; {notice}"));
    super::attached::attached_host_config(services, fallback).map_err(Into::into)
}

#[cfg(test)]
mod rebind_tests {
    use super::*;

    #[test]
    fn rebind_root_fallback_is_limited_to_an_unresolvable_source_selection() {
        for invalid_policy in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let paths =
                crate::home::paths::PlakPaths::for_workspace(root.path().join("home"), root.path());
            paths.ensure_layout().unwrap();
            if invalid_policy {
                std::fs::write(
                    paths.config_file(),
                    "[orchestration]\nmax_children = 'not a number'\n",
                )
                .unwrap();
            }
            let services = crate::shell::services::Services::for_home(paths);
            let mut source = product_sessions::SessionRecord::new(root.path().to_path_buf());
            source.account = Some("removed-account".into());
            source.model = Some("removed-model".into());
            source.effort = Some("medium".into());
            let mut history = plakous::sessions::PortableSessionHistory::new(
                Vec::new(),
                "source-replay".into(),
                plakous::model::ModelId::new("removed-model").unwrap(),
            );
            let mut note = "source selected".to_owned();
            let fallback = product_config::ProductConfig::default_product();
            let result = import_host_config(
                &services,
                source.clone(),
                Some(&fallback),
                &mut history,
                &mut note,
            );
            let error = result
                .err()
                .expect("unconfigured root or invalid policy refuses before creation");
            if invalid_policy {
                assert!(!note.contains("using the configured root"), "{note}");
                assert_eq!(history.replay_account(), "source-replay");
                assert!(!error.message().contains("removed-account"), "{error}");
            } else {
                assert!(
                    note.contains("account `removed-account` is no longer available"),
                    "{note}"
                );
                assert!(note.contains("using the configured root binding"), "{note}");
                assert_eq!(history.replay_account(), "");
            }
            assert_eq!(source.account.as_deref(), Some("removed-account"));
            assert_eq!(services.product_store_opens(), 0);
        }
    }
}
