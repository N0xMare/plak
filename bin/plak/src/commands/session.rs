//! Session command families: lookup, recovery, delete, import, and attach.

mod attached;
mod binding;
mod delete;
pub(crate) mod import_export;
mod lookup;
mod read_only;
#[cfg(any(test, feature = "tui"))]
pub(crate) mod rebind;
mod recovery;

pub(crate) use attached::{
    probe_carry_capacity, probe_prepared_capacity, run_attached_action, verify_exact_reopen,
};

pub use binding::seed_new_session_binding;
pub(crate) use binding::{ROOT_BINDING_REMEDY, resolve_api_composition};

pub use delete::confirm_session_mutation;
pub(crate) use delete::{delete_session, gc_sessions};

pub(crate) use import_export::{
    PreparedImport, export_session, import_prepared, import_reload_handoff, import_session,
    request_reload,
};

pub use lookup::load_exact_product_session;
#[cfg(feature = "tui")]
pub(crate) use lookup::match_session_id_prefix;
pub(crate) use lookup::{
    MIN_SESSION_ID_PREFIX, load_admitted_product_session, resolve_read_only_session_id,
};

pub(crate) use read_only::{log_child_read_only, parse_read_address, show_selected_child};

pub(crate) use recovery::{show_child_recovery_status, show_recovery_status};
