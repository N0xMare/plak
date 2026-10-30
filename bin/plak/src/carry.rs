//! Session carry-forward across builds with different state keys.
//!
//! A build never reads another build's stores as session authority. The one
//! thing that crosses is a portable session reference: `plak session export` writes it
//! from this build's state, `plak session import` creates a new linked session
//! from the full neutral conversation and named state. This build imports
//! versions 11 and 10; v10 notice text stays verbatim without inferred remedies.

mod document;
#[cfg(any(test, feature = "tui"))]
mod epoch;
mod export;
mod handoff;
mod history;
mod import;
#[cfg(any(test, feature = "tui"))]
mod interactive;
#[cfg(any(test, feature = "tui"))]
pub(crate) mod rebind;
mod reference;
mod reload;

pub(crate) use document::{
    CarryDocument, IMPORTABLE_VERSIONS, VERSION, read_document, write_document,
};

pub(crate) use export::{export_session, new_carry_file};
pub(crate) use handoff::{ReloadHandoff, take_reload_handoff};
pub(crate) use reload::{
    CarryCapacityProbe, ReloadProbe, ReopenProbe, reload_probe, reopen_credential_gate,
    reopen_probe,
};
#[cfg(feature = "tui")]
pub(crate) use {
    epoch::apply_on_reload,
    interactive::{
        ReloadPlan, ReloadTarget, carry_foreign_session, carry_out, reload_plan,
        reload_target_for_session,
    },
};

pub(crate) use import::{CarriedImport, already_carried, carried_forward_label, import_and_resume};

pub(crate) use reference::remove_retained_archive;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
