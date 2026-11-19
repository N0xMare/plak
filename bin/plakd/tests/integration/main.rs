//! Product integration contracts.

mod cli_contract;
#[cfg(not(feature = "test-hooks"))]
mod client_fixture_contract;
#[cfg(all(feature = "test-hooks", feature = "maintainer"))]
mod crash_sweep;
mod diagnostics_contract;
#[cfg(all(
    feature = "test-hooks",
    feature = "codex",
    feature = "claude",
    feature = "grok"
))]
mod gateway_admin;
#[path = "support/gateway_process.rs"]
mod gateway_process;
#[cfg(all(
    feature = "test-hooks",
    feature = "codex",
    feature = "claude",
    feature = "grok"
))]
#[path = "support/gateway_seed.rs"]
mod gateway_seed;
mod gateway_service;
#[cfg(all(target_os = "linux", feature = "web"))]
mod host_install;
#[cfg(all(target_os = "macos", feature = "web"))]
mod host_install_launchd;
#[cfg(all(feature = "test-hooks", feature = "web"))]
mod host_skeleton;
#[cfg(feature = "test-hooks")]
mod lingering_owner;
#[cfg(feature = "test-hooks")]
mod offline_e2e;
#[cfg(feature = "test-hooks")]
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
#[cfg(all(feature = "test-hooks", feature = "web"))]
mod web_endpoint;
#[cfg(feature = "test-hooks")]
mod wire_client;
