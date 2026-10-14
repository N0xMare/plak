//! Serialized observation publication.

use super::{
    AgentEvent, AgentEventKind, Arc, EventContext, EventHub, EventId, Mutex, PreparedAgentEvent,
    ViewError, lock,
};

use crate::{AgentError, AgentErrorKind};

/// Bounded best-effort process-local observation publication.
#[derive(Clone)]
pub(crate) struct EventPipeline {
    hub: EventHub,
    publication: Arc<Mutex<()>>,
}

impl EventPipeline {
    pub(crate) fn new(hub: EventHub) -> Self {
        Self {
            hub,
            publication: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn publish(
        &self,
        context: EventContext,
        kind: AgentEventKind,
    ) -> Result<Arc<AgentEvent>, AgentError> {
        let _publication = lock(&self.publication);
        let event_id = self.hub.reserve_event_id().map_err(pipeline_view_error)?;
        let prepared = self
            .hub
            .prepare_reserved(context, event_id, kind)
            .map_err(pipeline_view_error)?;
        self.publish_prepared_locked(prepared, false)
    }

    pub(crate) fn publish_with(
        &self,
        context: EventContext,
        create: impl FnOnce(EventId) -> AgentEventKind,
    ) -> Result<Arc<AgentEvent>, AgentError> {
        let _publication = lock(&self.publication);
        let event_id = self.hub.reserve_event_id().map_err(pipeline_view_error)?;
        let prepared = self
            .hub
            .prepare_reserved(context, event_id, create(event_id))
            .map_err(pipeline_view_error)?;
        self.publish_prepared_locked(prepared, false)
    }

    pub(crate) fn close(&self) -> Result<(), AgentError> {
        let _publication = lock(&self.publication);
        if lock(&self.hub.inner.state).closed {
            return Ok(());
        }
        let prepared = self
            .hub
            .prepare_reserved(
                EventContext::agent(),
                self.hub.inner.close_event_id,
                AgentEventKind::AgentClosed,
            )
            .map_err(pipeline_view_error)?;
        self.publish_prepared_locked(prepared, true)?;
        Ok(())
    }

    fn publish_prepared_locked(
        &self,
        prepared: PreparedAgentEvent,
        closes: bool,
    ) -> Result<Arc<AgentEvent>, AgentError> {
        self.hub
            .publish_prepared(prepared, closes)
            .map_err(pipeline_view_error)
    }
}

fn pipeline_view_error(error: ViewError) -> AgentError {
    let kind = match error {
        ViewError::Identity => AgentErrorKind::Identity,
        ViewError::Closed => AgentErrorKind::Shutdown,
        ViewError::SubscriberLimit
        | ViewError::IncompatibleCursor
        | ViewError::CursorAhead
        | ViewError::InvalidEvent
        | ViewError::FenceSaturated
        | ViewError::FenceStateUnavailable => AgentErrorKind::Internal,
    };
    AgentError::new(kind, error.to_string())
}
