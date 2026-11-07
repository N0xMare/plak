//! Session carry-forward across builds with different state keys.
//!
//! A build never reads another build's stores as session authority. The one
//! thing that crosses is a portable session reference: `plak session export` writes it
//! from this build's state, `plak session import` creates a new linked session
//! from the full neutral conversation and named state. This build imports
//! versions 12 and 11; a v11 document lands on the local runner under the home
//! default profile.

mod document;
#[cfg(any(test, feature = "tui"))]
mod epoch;
mod export;
pub(crate) mod handoff;
mod history;
mod import;
pub(crate) mod intent;
#[cfg(any(test, feature = "tui"))]
mod interactive;
pub(crate) mod rebind;
mod reference;
mod reload;

pub(crate) use document::{
    CarryDocument, IMPORTABLE_VERSIONS, VERSION, read_document, write_document,
};

pub(crate) use export::{export_session, new_carry_file};
pub(crate) use handoff::take_reload_handoff;
pub(crate) use reload::{
    CarryCapacityProbe, ReloadProbe, ReloadTarget, ReopenProbe, reload_probe,
    reopen_credential_gate, reopen_probe,
};
#[cfg(feature = "tui")]
pub(crate) use {
    epoch::apply_on_reload,
    interactive::{
        ReloadPlan, carry_foreign_session, carry_out, reload_plan, reverify_released, switch_to,
    },
    rebind::rebind_session,
};

/// A running turn refuses a reload or rebind whose successor is another
/// session: only a reload that resumes the same session hands it off.
pub(crate) const CARRY_REFUSED_TURN: &str =
    "reload refused: wait for the turn to finish or cancel it";

/// Running sub-agents refuse a reload or rebind whose successor is another
/// session, for the same reason.
pub(crate) const CARRY_REFUSED_CHILDREN: &str = "reload refused: sub-agents are still running";

/// The installed build a reload of `session_id` would start, and this
/// build's refusal of it, resolved while the session is still attached.
/// `in_flight` is the work a running turn or sub-agents leave, which a
/// reload resuming the same session hands off (spec 02 §5.1) and one
/// carrying into another session refuses.
#[cfg(feature = "tui")]
pub(crate) fn probe_reload(
    paths: &crate::home::paths::PlakPaths,
    workspace: &std::path::Path,
    session_id: Option<&str>,
    in_flight: Option<&'static str>,
) -> (Option<ReloadTarget>, Option<String>) {
    let target = interactive::reload_target_for_session(paths, workspace, session_id);
    let refusal = target.as_ref().and_then(|target| {
        interactive::reload_refusal(target, &crate::home::state::state_key(), in_flight)
    });
    (target, refusal)
}

/// A build with no interactive face has no installed build to reload onto.
#[cfg(not(feature = "tui"))]
pub(crate) const fn probe_reload(
    _paths: &crate::home::paths::PlakPaths,
    _workspace: &std::path::Path,
    _session_id: Option<&str>,
    _in_flight: Option<&'static str>,
) -> (Option<ReloadTarget>, Option<String>) {
    (None, None)
}

pub(crate) use import::{CarriedImport, already_carried, carried_forward_label, import_and_resume};
pub(crate) use intent::{load_for_work, pending_successor, work_id};

pub(crate) use reference::remove_retained_archive;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
