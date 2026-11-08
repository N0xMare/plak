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

/// `plak session reload <id> --expect-binary <binary>`: queues one request
/// that the interactive host running this session reloads onto the installed
/// build at its next idle boundary, and prints where the outcome lands.
pub(crate) fn request_reload(
    services: &crate::shell::services::Services,
    id: &str,
    expect_binary: &str,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    // The announcement, not this build's store, locates the session: its host
    // may run another state key.
    let (session, outcome) = crate::reload_request::request(
        services.paths()?,
        id,
        expect_binary,
        crate::build_identity::binary_id(),
    )
    .map_err(CliFailure::new)?;
    writeln!(
        stdout,
        "reload requested for session {session} onto build {expect_binary}; it runs when the session is idle; outcome: {}",
        outcome.display()
    )
    .map_err(|_| CliFailure::new("stdout failed"))
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
    /// The source session's own profile, when the caller knows it; otherwise
    /// a same-store source's tree answers, and failing that the home default
    /// applies.
    pub(super) profile: Option<crate::profiles::SessionSelection>,
    /// A same-store carry (binding repair or orchestration apply) continues
    /// its source's agent line and is written ahead of its runtime create;
    /// every other import starts a new line.
    pub(super) carry: Option<LineCarry>,
}

/// A same-store carry's write-ahead facts (spec 02 §2.6).
pub(crate) struct LineCarry {
    /// The notice the successor records.
    pub(crate) cause: String,
    /// The reload's orchestration carry, which has no root fallback.
    pub(crate) orchestration: bool,
    /// The intent record an interrupted carry left, resumed under its own
    /// id and create operation instead of a new one.
    pub(crate) resume: Option<product_sessions::SessionRecord>,
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
            profile: None,
            carry: None,
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

/// The `--import-resume` half of a reload handoff: the source names the
/// session's own profile and, when it carries a changed binding, why.
pub(crate) fn import_reload_handoff(
    services: &crate::shell::services::Services,
    document: crate::carry::CarryDocument,
    local_handoff: bool,
    cause: Option<&str>,
    profile: Option<crate::profiles::SessionSelection>,
) -> CliResult<(product_sessions::SessionRecord, String)> {
    let estimated_tokens = document.estimated_tokens();
    let messages = document.message_count();
    let carried = match cause {
        Some(cause) => document.into_noticed_import(cause, local_handoff)?,
        None => document
            .into_import_with_replay(local_handoff)
            .map_err(|e| CliFailure::new(e.to_string()))?,
    };
    crate::build_features::admit_session(&carried.record)?;
    import_prepared(
        services,
        PreparedImport {
            carried,
            estimated_tokens,
            messages,
            profile,
            carry: None,
        },
    )
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
        profile,
        carry,
    } = prepared;
    let crate::carry::CarriedImport {
        mut record,
        mut history,
        mut note,
        profile: document_profile,
    } = carried;
    let placement = super::import_placement::ImportPlacement::prepare(
        services,
        record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str()),
        &record.workspace_root,
    )?;
    let services = &placement.services;
    let resumed = carry.as_ref().and_then(|carry| carry.resume.as_ref());
    if let Some(intent) = resumed {
        resume_identity(&mut record, &mut note, intent);
    }
    let paths = super::attached::session_paths(services, &record)?;
    product_config::SessionPolicy::load(&paths).map_err(CliFailure::new)?;
    let (source_id, source_key) = record
        .carried_forward_from()
        .ok_or_else(|| CliFailure::new("missing carry lineage"))?;
    let (source_id, source_key) = (source_id.to_owned(), source_key.to_owned());
    refuse_carried_twice(services, &record, &source_id, &source_key, resumed)?;
    record.validate_for_store().map_err(CliFailure::new)?;
    let mut config = import_host_config(services, record, root_fallback, &mut history, &mut note)?;
    let profile = profile.or(document_profile);
    keep_source_profile(&paths, &mut config, profile.as_ref(), &mut note);
    config.seed_history = Some(history);
    let line = seed_line(services, carry.is_some(), &source_id, &source_key);
    if let (Some(carry), plakous_hosted::agent::SeededLine::Continue(source)) = (&carry, &line) {
        write_ahead(&mut config, carry, *source)?;
    }
    config.seed_line = Some(line);
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
    let projected = project_imported_session(services, &paths, &record);
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

fn project_imported_session(
    services: &crate::shell::services::Services,
    paths: &crate::home::paths::PlakPaths,
    record: &product_sessions::SessionRecord,
) -> CliResult<()> {
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|_| CliFailure::new("application executor failed"))?;
    let sessions = services
        .state_roots_on(
            paths,
            record
                .runner
                .as_ref()
                .map_or("local", |runner| runner.runner_id.as_str()),
        )?
        .runtime_sessions()?;
    let records = services.session_records()?;
    executor
        .block_on(product_host::project_session(&sessions, &records, record))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map(|_| ())
        .map_err(CliFailure::new)
}

/// Refuses a second import of a source this workspace already carried
/// forward, unless it is the interrupted carry being resumed.
fn refuse_carried_twice(
    services: &crate::shell::services::Services,
    record: &product_sessions::SessionRecord,
    source_id: &str,
    source_key: &str,
    resumed: Option<&product_sessions::SessionRecord>,
) -> CliResult<()> {
    if let Some(existing) = crate::carry::already_carried(
        services,
        &record.workspace_root,
        record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str()),
        source_id,
        source_key,
    )
    .map_err(CliFailure::new)?
        && resumed.is_none_or(|intent| intent.id != existing.id)
    {
        return Err(CliFailure::new(format!(
            "session {source_id} ({source_key}) was already carried forward as {}; resume it with `plak --resume {}`",
            existing.id, existing.id
        )));
    }
    Ok(())
}

/// An interrupted carry finishes under its own identity, so its create
/// replays exactly.
fn resume_identity(
    record: &mut product_sessions::SessionRecord,
    note: &mut String,
    intent: &product_sessions::SessionRecord,
) {
    *note = note.replace(&record.id, &intent.id);
    record.id.clone_from(&intent.id);
    record.node.clone_from(&intent.node);
    record
        .create_operation_id
        .clone_from(&intent.create_operation_id);
    record.created_at_unix = intent.created_at_unix;
    record.store_revision = intent.store_revision;
}

/// Saves a same-store carry's successor ahead of its runtime create: a crash
/// after the create leaves a record that names its own create operation and
/// source, so open and startup can finish, replay or withdraw it (spec 02
/// §2.6).
fn write_ahead(
    config: &mut product_host::HostConfig<crate::surface_protocol::PlakExtension>,
    carry: &LineCarry,
    source: plakous::sessions::SessionLocator,
) -> CliResult<()> {
    config.session.carry_intent = Some(plakous_hosted::sessions::CarryIntent {
        source,
        continues_line: true,
        cause: carry.cause.clone(),
        orchestration: carry.orchestration,
    });
    config.session.touch();
    config
        .store
        .save(&mut config.session)
        .map_err(CliFailure::new)
}

/// The agent line a carried session joins: a same-store carry continues its
/// source's line, and any other import starts a new line naming its origin.
fn seed_line(
    services: &crate::shell::services::Services,
    continues_line: bool,
    source_id: &str,
    source_key: &str,
) -> plakous_hosted::agent::SeededLine {
    let source = (continues_line && source_key == crate::home::state::state_key())
        .then(|| {
            services
                .session_records()
                .ok()?
                .load(source_id)
                .ok()?
                .plakous_locator
        })
        .flatten();
    source.map_or_else(
        || plakous_hosted::agent::SeededLine::Imported(format!("{source_key}/{source_id}")),
        plakous_hosted::agent::SeededLine::Continue,
    )
}

/// The successor's genesis freezes its source's own profile, resolved against
/// the current template files, and its root tools follow that profile's root
/// template; the genesis records the profile the roster answers. A source that
/// records no profile, or whose templates no longer load, binds the home
/// default with one notice.
fn keep_source_profile(
    paths: &crate::home::paths::PlakPaths,
    config: &mut product_host::HostConfig<crate::surface_protocol::PlakExtension>,
    profile: Option<&crate::profiles::SessionSelection>,
    note: &mut String,
) {
    match successor_profile(paths, profile) {
        Ok((selected, profile)) => {
            config.root_template = selected.root_template_fact();
            config.session.root_profile = Some(plakous_hosted::sessions::RootProfile {
                root_template: profile.root.clone(),
                working_set: profile.working_set.clone(),
            });
        }
        // The import's summary line carries it once: the CLI prints it and the
        // TUI opens the successor with it as its opening note.
        Err(notice) => note.push_str(&format!("; {notice}")),
    }
}

/// The configuration a successor freezes for its source's `profile`, or the
/// notice saying why the home default applies instead.
fn successor_profile<'a>(
    paths: &crate::home::paths::PlakPaths,
    profile: Option<&'a crate::profiles::SessionSelection>,
) -> Result<
    (
        product_config::ProductConfig,
        &'a crate::profiles::SessionSelection,
    ),
    String,
> {
    let Some(profile) = profile else {
        return Err(
            "this session's export records no profile; the home default profile applies".to_owned(),
        );
    };
    product_config::ProductConfig::load(paths)
        .and_then(|home| crate::profiles::selection_config(&home, profile))
        .map(|selected| (selected, profile))
        .map_err(|error| {
            format!(
                "this session's profile could not be loaded ({}); the home default profile applies",
                plakous::model::SafeDiagnostic::new(error)
            )
        })
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
        Err(error) => return Err(error.into()),
    };
    let Some(root) = root_fallback else {
        return Err(source_error);
    };
    let mut fallback = record.clone();
    fallback.target_selection = product_config::ProductTargetSelection::Native;
    let paths = services.paths()?;
    let context = if record.runner.is_some() {
        paths.for_remote_workspace(&record.workspace_root)
    } else {
        paths.for_workspace(&record.workspace_root)
    };
    let roots = services.state_roots_on(
        &context,
        record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str()),
    )?;
    super::binding::seed_new_session_binding_on(
        &mut fallback,
        root,
        roots.workspace_probe().as_ref(),
    )?;
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
    fn a_successor_without_its_sources_profile_says_the_home_default_applies() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::home::paths::PlakPaths::explicit(root.path().join("home"))
            .for_workspace(root.path());
        paths.ensure_layout().unwrap();
        let none = successor_profile(&paths, None).err().unwrap();
        assert_eq!(
            none,
            "this session's export records no profile; the home default profile applies"
        );
        let missing = crate::profiles::SessionSelection {
            root: Some("gone-root".to_owned()),
            working_set: Vec::new(),
        };
        let unloadable = successor_profile(&paths, Some(&missing)).err().unwrap();
        assert!(
            unloadable.starts_with("this session's profile could not be loaded (")
                && unloadable.contains("gone-root"),
            "{unloadable}"
        );
    }

    #[test]
    fn rebind_root_fallback_is_limited_to_an_unresolvable_source_selection() {
        for invalid_policy in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let paths = crate::home::paths::PlakPaths::explicit(root.path().join("home"))
                .for_workspace(root.path());
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
                    note.contains("unknown_account: `removed-account`"),
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
