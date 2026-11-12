//! Product integration contracts.

mod cli_contract;
#[cfg(not(feature = "test-hooks"))]
mod client_fixture_contract;
#[cfg(all(feature = "test-hooks", feature = "maintainer"))]
mod crash_sweep;
mod diagnostics_contract;
#[cfg(all(target_os = "linux", feature = "web"))]
mod host_install;
#[cfg(all(feature = "test-hooks", feature = "web"))]
mod host_skeleton;
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
#[cfg(unix)]
mod runner_contract;
#[cfg(feature = "test-support")]
mod telemetry_contract;
#[cfg(all(feature = "test-hooks", feature = "web"))]
mod web_endpoint;
#[cfg(feature = "test-hooks")]
mod wire_client;
