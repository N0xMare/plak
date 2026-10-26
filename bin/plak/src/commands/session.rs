//! Session command families: lookup, recovery, delete, import, and attach.

mod attached;
mod binding;
mod delete;
mod import_export;
mod lookup;
mod read_only;
mod recovery;

pub(crate) use attached::{probe_carry_capacity, probe_prepared_capacity, run_attached_action};

pub(crate) use binding::resolve_api_composition;
pub use binding::{repair_invalid_new_session_binding, seed_new_session_binding};

pub use delete::confirm_session_mutation;
pub(crate) use delete::{delete_session, gc_sessions};

pub(crate) use import_export::{
    PreparedImport, export_session, import_document, import_prepared, import_session,
    rebind_document,
};

pub use lookup::load_exact_product_session;
#[cfg(feature = "tui")]
pub(crate) use lookup::match_session_id_prefix;
pub(crate) use lookup::{
    MIN_SESSION_ID_PREFIX, load_admitted_product_session, resolve_read_only_session_id,
};

pub(crate) use read_only::{log_child_read_only, parse_read_address, show_selected_child};

pub(crate) use recovery::{show_child_recovery_status, show_recovery_status};
