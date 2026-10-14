//! Refresh queue authority without waiting for a busy turn to end.

use super::{Duration, HostEvent, Instant, TurnDrive, emit_queue_snapshot, sessions};

pub(super) struct QueueRefresh {
    expected: sessions::QueueCommands,
    pub(super) task: tokio::task::JoinHandle<Result<sessions::QueueCommands, String>>,
}

impl<P: plakous_host_protocol::ProtocolExtension> TurnDrive<'_, P> {
    pub(super) async fn poll_queue_projection(&mut self) {
        if self
            .queue_refresh
            .as_ref()
            .is_some_and(|refresh| refresh.task.is_finished())
            && let Some(refresh) = self.queue_refresh.take()
            && let Ok(Ok(commands)) = refresh.task.await
            // A read started before a new immutable submission cannot unlock it.
            && refresh.expected == self.host.session.queue_commands
        {
            self.apply_queue_join(commands);
        }
        if Instant::now() < self.queue_refresh_at {
            return;
        }
        self.queue_refresh_at = Instant::now() + Duration::from_millis(250);
        if let Some(live) = self.host.live_projector.as_mut() {
            live.tick(self.host.live_target.clone());
        }
        if self.queue_refresh.is_none()
            && (self.host.session.queue_projection_debt
                || self.host.session.queue_commands.has_pending())
        {
            let store = self.host.projection_store.clone();
            let id = self.host.session.id.clone();
            self.queue_refresh = Some(QueueRefresh {
                expected: self.host.session.queue_commands.clone(),
                task: tokio::spawn(async move {
                    super::super::projection::queue_join(&store, &id).await
                }),
            });
        }
    }

    pub(super) fn apply_queue_join(&mut self, commands: sessions::QueueCommands) {
        let changed = self.host.session.queue_commands != commands;
        self.host.session.queue_commands = commands;
        self.host.session.queue_projection_debt &= self.host.session.queue_commands.has_pending();
        self.host
            .session
            .pending_queue
            .retain(|item| !self.host.session.queue_commands.is_consumed(item));
        if !self.host.session.queue_projection_debt
            && !self.host.session.queue_commands.has_pending()
        {
            self.queue_wait_noted = false;
        }
        if changed {
            emit_queue_snapshot(self.host.events, self.host.session);
        }
    }

    pub(super) fn note_queue_projection_wait(&mut self) {
        emit_queue_snapshot(self.host.events, self.host.session);
        if !self.queue_wait_noted {
            self.queue_wait_noted = true;
            let _ = self.host.events.send(HostEvent::Status {
                phase: "running".into(),
                queue_len: self.host.session.pending_queue.len(),
                detail: Some("queue projection is catching up; prompt remains queued".into()),
            });
            let _ = self.host.events.notice(sessions::TranscriptRow::info(
                sessions::NoticeScope::Queue,
                "queue projection is catching up; prompt remains queued",
            ));
        }
    }
}
