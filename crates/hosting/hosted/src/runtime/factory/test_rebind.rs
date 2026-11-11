//! Resolve only an explicitly registered scripted profile transport.

use crate::runtime::{HostConfig, models::effort_from_config, test_kit::ScriptedRuntimeFactory};
use crate::{CliResult, binding::TargetSelection};
use plakous::sessions::SessionTargetBinding;
use plakous::subscriptions::NativeProvider;
use std::any::Any;

pub(in crate::runtime) fn registered_target<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    account: &str,
    model: &str,
    effort: Option<&str>,
    selection: &TargetSelection,
) -> CliResult<Option<(NativeProvider, SessionTargetBinding)>> {
    let Some(scripted) =
        (&*config.runtime_factory as &dyn Any).downcast_ref::<ScriptedRuntimeFactory>()
    else {
        return Ok(config.runtime_factory.target_binding());
    };
    let (effort, explicit) = effort_from_config(effort);
    scripted.target_binding_for(account, model, selection, explicit.then_some(effort))
}
