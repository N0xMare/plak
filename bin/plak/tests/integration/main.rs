//! Product integration contracts.

mod cli_contract;
#[cfg(all(feature = "test-hooks", feature = "maintainer"))]
mod crash_sweep;
mod diagnostics_contract;
#[cfg(feature = "test-hooks")]
mod lingering_owner;
#[cfg(feature = "test-hooks")]
mod offline_e2e;
#[cfg(feature = "test-hooks")]
#[path = "support/offline.rs"]
mod offline_support;
#[cfg(feature = "test-hooks")]
mod offline_tui;
mod product_config_fixtures;
mod product_doctor_contract;
#[cfg(feature = "maintainer")]
mod reload_rehearsal;
#[cfg(feature = "test-support")]
mod telemetry_contract;
