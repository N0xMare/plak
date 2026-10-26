//! Session carry-forward across builds with different state keys.
//!
//! A build never reads another build's stores as session authority. The one
//! thing that crosses is a portable session reference: `plak session export` writes it
//! from this build's state, `plak session import` creates a new linked session
//! from the full neutral conversation and named state. Version 8 imports with
//! empty named state; version 7 is no longer importable.

mod document;
mod export;
mod history;
mod import;
mod reference;
mod reload;

pub(crate) use document::{
    CarryDocument, IMPORTABLE_VERSIONS, VERSION, read_document, write_document,
};

#[cfg(feature = "tui")]
pub(crate) use export::carry_out;
pub(crate) use export::{export_session, new_carry_file};
pub(crate) use reload::{CarryCapacityProbe, ReloadProbe, reload_probe};
#[cfg(feature = "tui")]
pub(crate) use reload::{ReloadPlan, ReloadTarget, reload_plan, reload_target_for_session};

#[cfg(feature = "tui")]
pub(crate) use import::carry_foreign_session;
pub(crate) use import::{CarriedImport, already_carried, carried_forward_label, import_and_resume};

pub(crate) use reference::remove_retained_archive;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
