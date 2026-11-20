pub(crate) const fn offline_model_selected() -> bool {
    false
}

pub(crate) const fn snapshot_fixture() -> bool {
    false
}

pub(crate) const fn product_store_schema(
    base: plakous_recursive_subagents_store::admission::Schema,
) -> plakous_recursive_subagents_store::admission::Schema {
    base
}

pub(crate) const fn gateway_review_clock(
    subscriptions: plakous::subscriptions::NativeSubscriptions,
    _provider: plakous::subscriptions::NativeProvider,
) -> plakous::subscriptions::NativeSubscriptions {
    subscriptions
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "same seam when test hooks are disabled"
)]
pub(crate) const fn gateway_loopback_anthropic<S: plakous::gateway::server::GatewayService>(
    server: plakous::gateway::server::RelayServer<S>,
) -> Result<plakous::gateway::server::RelayServer<S>, String> {
    Ok(server)
}

#[cfg(feature = "web")]
#[path = "host_lifecycle.rs"]
pub(crate) mod host_lifecycle;
pub(crate) const fn binding_executor_cut() -> bool {
    false
}
pub(crate) fn upgrade_state_key() -> Option<String> {
    None
}
pub(crate) fn upgrade_input_limit() -> Option<String> {
    None
}
pub(crate) const fn upgrade_import_refused() -> bool {
    false
}

pub(crate) const fn upgrade_runner_revision() -> Option<String> {
    None
}
#[allow(
    dead_code,
    reason = "only the optional host can exercise same-image handoff"
)]
pub(crate) const fn force_host_upgrade() -> bool {
    false
}
#[allow(dead_code, reason = "only the optional host coordinates upgrades")]
pub(crate) fn upgrade_crash_cut(_name: &str) {}
#[allow(dead_code, reason = "only the optional host prepares upgrades")]
pub(crate) const fn upgrade_preparation_refused(_name: &str) -> bool {
    false
}
#[allow(dead_code, reason = "only the optional host starts replacement images")]
pub(crate) fn upgrade_key_environment(_key: &str) -> impl Iterator<Item = (&'static str, String)> {
    std::iter::empty()
}
pub(crate) const fn profile_route_ready(
    _catalog: &[plakous_host_protocol::views::ProviderSection],
    _account: &str,
    _model: &str,
) -> Option<bool> {
    None
}
pub(crate) const fn ready_accounts() -> Option<Vec<String>> {
    None
}
pub(crate) fn refresh_profile_ready_catalog(
    sections: Vec<plakous_host_protocol::views::ProviderSection>,
) -> Vec<plakous_host_protocol::views::ProviderSection> {
    sections
}

#[allow(dead_code, reason = "only builds with the host select startup inputs")]
pub(crate) fn host_start_environment() -> Vec<(&'static str, std::ffi::OsString)> {
    Vec::new()
}
#[allow(dead_code, reason = "only the optional host dials its local runner")]
pub(crate) fn runner_windows() -> plakous::runner::ClientWindows {
    plakous::runner::ClientWindows::default()
}
pub(crate) const fn runner_reconnect_window(default: std::time::Duration) -> std::time::Duration {
    default
}
pub(crate) fn runner_before_ready(
    _client: &plakous::runner::RunnerClient,
) -> std::future::Ready<Result<(), plakous::runner::PlacementFailure>> {
    std::future::ready(Ok(()))
}
pub(crate) fn runner_ready_result(_result: &Result<(), plakous::runner::PlacementFailure>) {}
pub(crate) fn runner_link(
    client: plakous::runner::RunnerClient,
) -> std::sync::Arc<dyn plakous::tools::runner::RunnerLink> {
    std::sync::Arc::new(client)
}
pub(crate) fn runner_lease_clock(
    _home: &std::path::Path,
) -> Option<std::sync::Arc<plakous::runner::RunnerLeaseClock>> {
    None
}
pub(crate) const fn runner_services() -> Option<plakous::runner::RunnerServices> {
    None
}
#[allow(dead_code, reason = "headless builds do not start hosts")]
pub(crate) const fn runner_detached() -> bool {
    false
}
#[allow(dead_code, reason = "headless builds do not start hosts")]
pub(crate) const fn host_idle_exit_seconds() -> Option<std::ffi::OsString> {
    None
}
