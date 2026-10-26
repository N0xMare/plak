use super::{AgentSessions, Arc};
pub(super) fn recorded_sidecars(
    _: &AgentSessions,
    _: plakous::sessions::SessionLocator,
) -> std::future::Ready<
    Result<Option<Arc<dyn plakous::tools::SidecarLocator>>, plakous::sessions::AgentSessionsError>,
> {
    std::future::ready(Ok(None))
}
