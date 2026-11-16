//! Ordinary builds expose no scripted registration.

#[allow(
    clippy::unnecessary_wraps,
    reason = "the disabled scripted registration keeps the test factory's fallible lookup signature"
)]
pub(in crate::runtime) fn registered_target<P: plakous_host_protocol::ProtocolExtension>(
    config: &super::HostConfig<P>,
    _account: &str,
    _model: &str,
    _effort: Option<&str>,
    _selection: &crate::binding::TargetSelection,
) -> crate::CliResult<
    Option<(
        plakous::subscriptions::NativeProvider,
        plakous::sessions::SessionTargetBinding,
    )>,
> {
    Ok(config.runtime_factory.target_binding())
}
