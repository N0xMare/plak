//! Product integration contracts.

mod cli_contract;
#[cfg(not(feature = "test-support"))]
mod client_fixture_contract;
#[cfg(all(feature = "test-support", feature = "maintainer"))]
mod crash_sweep;
#[cfg(feature = "test-support")]
#[path = "support/lifetime.rs"]
mod daemon_lifetime;
mod diagnostics_contract;
#[cfg(feature = "test-support")]
mod gateway_admin;
#[path = "support/gateway_process.rs"]
mod gateway_process;
#[cfg(feature = "test-support")]
#[path = "support/gateway_seed.rs"]
mod gateway_seed;
mod gateway_service;
#[cfg(all(target_os = "linux", feature = "web"))]
mod host_install;
#[cfg(all(target_os = "macos", feature = "web"))]
mod host_install_launchd;
#[cfg(all(feature = "test-support", feature = "web"))]
mod host_skeleton;
#[cfg(feature = "test-support")]
mod lingering_owner;
#[cfg(feature = "test-support")]
mod offline_e2e;
#[cfg(feature = "test-support")]
#[path = "support/offline.rs"]
mod offline_support;
mod product_config_fixtures;
mod product_doctor_contract;
#[cfg(feature = "maintainer")]
mod reload_rehearsal;
#[cfg(unix)]
mod runner_contract;
#[cfg(feature = "test-support")]
mod telemetry_contract;
#[cfg(all(feature = "test-support", feature = "web"))]
mod web_endpoint;
#[cfg(feature = "test-support")]
mod wire_client;
