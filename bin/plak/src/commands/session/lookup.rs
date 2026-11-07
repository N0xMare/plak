//! Exact and prefix session lookup.

use crate::store::sessions as product_sessions;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult};
use std::path::Path;

#[derive(Clone, Copy, Eq, PartialEq)]
enum LookupMode {
    Runtime,
    Work,
    Metadata,
}

pub(super) fn load_metadata_product_session(
    paths: &crate::home::paths::PlakPaths,
    store: &dyn crate::store::session_records::SessionRecordStore,
    session_id: &str,
) -> CliResult<product_sessions::SessionRecord> {
    load_product_session(paths, store, session_id, |_| Ok(()), LookupMode::Metadata)
}

pub fn load_exact_product_session(
    paths: &crate::home::paths::PlakPaths,
    store: &dyn crate::store::session_records::SessionRecordStore,
    session_id: &str,
) -> CliResult<product_sessions::SessionRecord> {
    load_product_session(paths, store, session_id, |_| Ok(()), LookupMode::Runtime)
}

pub(crate) fn load_admitted_product_session(
    paths: &crate::home::paths::PlakPaths,
    store: &dyn crate::store::session_records::SessionRecordStore,
    session_id: &str,
) -> CliResult<product_sessions::SessionRecord> {
    load_product_session(
        paths,
        store,
        session_id,
        crate::build_features::admit_session,
        LookupMode::Work,
    )
}

fn load_product_session(
    paths: &crate::home::paths::PlakPaths,
    store: &dyn crate::store::session_records::SessionRecordStore,
    session_id: &str,
    admit: impl FnOnce(&product_sessions::SessionRecord) -> CliResult<()>,
    mode: LookupMode,
) -> CliResult<product_sessions::SessionRecord> {
    let cwd = paths.workspace().map_err(CliFailure::new)?.to_path_buf();
    let record = (|| {
        let resolved = resolve_workspace_session_id(store, &cwd, session_id)?;
        store.load(&resolved).map_err(CliFailure::from)
    })()
    .map_err(|error| {
        if session_absent(error.message()) {
            crate::home::state::incompatible_session_refusal(paths, session_id)
                .map_or(error, CliFailure::new)
        } else {
            error
        }
    })?;
    admit(&record)?;
    if mode == LookupMode::Metadata {
        return Ok(record);
    }
    if mode == LookupMode::Work && record.archived {
        return Err(CliFailure::new(
            "session_archived: unarchive this session before admitting work",
        ));
    }
    let requested = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let recorded = std::fs::canonicalize(&record.workspace_root)
        .unwrap_or_else(|_| record.workspace_root.clone());
    if requested != recorded {
        return Err(CliFailure::new(format!(
            "session {session_id} belongs to workspace {}; run this command from there",
            recorded.display()
        )));
    }
    // A face that works on the session settles an interrupted carry's
    // record first (spec 02 §2.6), and creates the runtime session of one
    // no face has opened yet (`plak host`'s `new_session`); a read names
    // the exact record only.
    if record.plakous_locator.is_none() && mode == LookupMode::Runtime {
        return Err(CliFailure::new(
            "session has no exact canonical Plakous locator",
        ));
    }
    Ok(record)
}

/// Resolves the short id `plak resume`, the picker and the exit line print,
/// accepted wherever a session id is, among this workspace's sessions.
fn resolve_workspace_session_id(
    store: &dyn crate::store::session_records::SessionRecordStore,
    workspace: &Path,
    requested: &str,
) -> CliResult<String> {
    resolve_session_id(requested, workspace, |cwd| {
        store
            .list_for_workspace(cwd)
            .map(|sessions| sessions.into_iter().map(|session| session.id).collect())
    })
}

/// [`resolve_workspace_session_id`] for the read-only inspection commands,
/// which never open a writable store.
pub(crate) fn resolve_read_only_session_id(
    paths: &crate::home::paths::PlakPaths,
    requested: &str,
) -> CliResult<String> {
    let workspace = paths.workspace().map_err(CliFailure::new)?;
    resolve_session_id(requested, workspace, |cwd| {
        crate::store::ProductStore::session_ids_for_workspace_read_only(paths, cwd)
    })
    .map_err(|error| {
        if session_absent(error.message()) {
            crate::home::state::incompatible_session_refusal(paths, requested)
                .map_or(error, CliFailure::new)
        } else {
            error
        }
    })
}

/// Only absence in current authority permits a foreign-session lookup.
fn session_absent(message: &str) -> bool {
    message == "session not found"
        || message.starts_with("no session in this workspace starts with `")
}

fn resolve_session_id(
    requested: &str,
    workspace: &Path,
    workspace_ids: impl FnOnce(&Path) -> Result<Vec<String>, String>,
) -> CliResult<String> {
    if uuid::Uuid::parse_str(requested).is_ok_and(|id| id.hyphenated().to_string() == requested) {
        return Ok(requested.to_owned());
    }
    let ids = workspace_ids(workspace).map_err(CliFailure::new)?;
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    match_session_id_prefix(requested, &ids)
        .map(str::to_owned)
        .map_err(|message| CliFailure::classified(CliFailureKind::Usage, message))
}

/// Shortest session-id prefix accepted; below it a prefix says nothing.
pub(crate) const MIN_SESSION_ID_PREFIX: usize = 4;

/// Resolves a short session id among `ids`, refusing an ambiguous or unknown one.
pub(crate) fn match_session_id_prefix<'a>(
    prefix: &str,
    ids: &[&'a str],
) -> Result<&'a str, String> {
    if prefix.chars().count() < MIN_SESSION_ID_PREFIX {
        return Err(format!(
            "session id `{prefix}` is too short; give at least {MIN_SESSION_ID_PREFIX} characters or the full id"
        ));
    }
    let mut matches = ids.iter().copied().filter(|id| id.starts_with(prefix));
    match (matches.next(), matches.count()) {
        (Some(id), 0) => Ok(id),
        (Some(_), more) => Err(format!(
            "session id `{prefix}` matches {} sessions in this workspace; give more characters",
            more + 1
        )),
        (None, _) => Err(format!(
            "no session in this workspace starts with `{prefix}`; list with `plak session list` or `plak resume`"
        )),
    }
}
