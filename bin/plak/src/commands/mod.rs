//! Library-shaped composition for the product command workflows.
//!
//! The `plak` binary keeps clap parsing and dispatch, exit-code mapping,
//! output-mode selection and the TUI attach. Everything a command *does* —
//! subscription login/logout/refresh, catalog and model selection, run-loop
//! driving, session commands and attach-time host configuration — lives here.

pub mod agent;
pub mod application;
pub mod catalog;
pub(crate) mod doctor;
pub mod host_config;
pub mod login;
pub(crate) mod login_flow;
pub(crate) mod mcp;
pub(crate) mod mcp_login;
#[cfg(feature = "maintainer")]
pub(crate) mod reload_rehearsal;
pub(crate) mod run;
pub mod secret;
pub mod session;
pub mod support;
pub mod usage;

mod exchange_capture;
pub(crate) mod secret_input;
mod subscription_storage;
