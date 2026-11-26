//! Session carry-forward across builds with different state keys.
//!
//! A build never reads another build's stores as session authority. The one
//! thing that crosses is a portable session reference: `plakd session export` writes it
//! from this build's state, `plakd session import` creates a new linked session
//! from the full neutral conversation and named state. This build imports
//! versions 14 and 13, preserves their saved runner/profile, and refuses version
//! 12 and older. The writer emits version 14.

mod document;
mod export;
mod history;
mod import;
mod installed;
pub(crate) mod intent;
pub(crate) mod placement;
pub(crate) mod rebind;
mod reference;
mod reload;

pub(crate) use document::{
    CarryDocument, IMPORTABLE_VERSIONS, VERSION, private_dir, read_document, write_document,
    write_private_json,
};
pub(crate) use history::{TraceHistory, redact_payload};

pub(crate) use export::{export_session, new_carry_file};
pub(crate) use reload::{
    CarryCapacityProbe, REOPEN_HEAD_ADVANCED, ReloadProbe, ReloadTarget, ReopenProbe, check_reopen,
    child_reopen_probe, reload_probe, reopen_credential_gate, reopen_probe,
};
/// A running turn refuses a reload or rebind whose successor is another
/// session: only a reload that resumes the same session hands it off.
pub(crate) const CARRY_REFUSED_TURN: &str =
    "reload refused: wait for the turn to finish or cancel it";

/// Running sub-agents refuse a reload or rebind whose successor is another
/// session, for the same reason.
pub(crate) const CARRY_REFUSED_CHILDREN: &str = "reload refused: sub-agents are still running";

pub(crate) const OFFLINE_MIGRATION_REQUIRED: &str = "host_upgrade_offline_migration_required: a state-key change requires offline migration; settle work and stop the source host, use the source build to export sessions and the destination build to import them, then start the destination host";

pub(crate) fn state_key_valid(value: &str) -> bool {
    let mut parts = value.split('.');
    for prefix in ['r', 'e', 'p', 't'] {
        if parts
            .next()
            .and_then(|part| part.strip_prefix(prefix))
            .is_none_or(|number| {
                number.is_empty()
                    || !number.bytes().all(|byte| byte.is_ascii_digit())
                    || number.parse::<u32>().is_err()
            })
        {
            return false;
        }
    }
    parts.next().is_none()
}

/// The installed build a reload of `session_id` would start, and this
/// build's refusal of it, resolved while the session is still attached.
/// `in_flight` is the work a running turn or sub-agents leave, which a
/// reload resuming the same session hands off (spec 02 §4.6) and one
/// carrying into another session refuses.
pub(crate) fn probe_reload(
    paths: &crate::home::paths::PlakPaths,
    workspace: &std::path::Path,
    session_id: Option<&str>,
    in_flight: Option<&'static str>,
) -> (Option<ReloadTarget>, Option<String>) {
    let target = installed::reload_target_for_session(paths, workspace, session_id);
    let refusal = target.as_ref().and_then(|target| {
        installed::reload_refusal(target, &crate::home::state::state_key(), in_flight)
    });
    (target, refusal)
}

pub(crate) use import::{CarriedImport, already_carried, carried_forward_label};
pub(crate) use intent::{load_for_work, work_id};

pub(crate) use reference::remove_retained_archive;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
