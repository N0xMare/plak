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
        self.poll_mail_notice();
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

    fn poll_mail_notice(&mut self) {
        if !self.host.host_wakes || self.host.session.child_wake_suppressed {
            self.host.job_agent.set_unread_mail_pending(false);
            return;
        }
        if let Some(reply) = self.notice_reply.as_mut() {
            match reply.try_recv() {
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
                _ => self.notice_reply = None,
            }
        }
        let checkpoint = self.host.job_agent.checkpoint_id();
        let tree_id =
            super::super::topology_state::durable_tree_id(self.host.rows, self.host.session);
        let Some(runtime) = crate::topology_runtime::lookup(&tree_id) else {
            return;
        };
        let Ok(ids) = runtime.read(|tree| {
            tree.unread_mail(&tree.root_agent_id)
                .map(|mail| mail.message_id.clone())
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        self.host.job_agent.set_unread_mail_pending(!ids.is_empty());
        if ids.is_empty() {
            return;
        }
        let repeat = self.host.job_agent.has_repeat_process_observation();
        if self.mail_batch_notified && (!repeat || self.mail_repeat_notified) {
            return;
        }
        let identity = checkpoint.to_string();
        if self.noticed_mail_checkpoint.as_ref() == Some(&identity)
            && ids.iter().all(|id| self.noticed_mail.contains(id))
        {
            return;
        }
        if let Ok(reply) = self.control.notice_at(checkpoint) {
            self.notice_reply = Some(reply);
            self.noticed_mail_checkpoint = Some(identity);
            self.noticed_mail = ids;
            if self.mail_batch_notified {
                self.mail_repeat_notified = true;
            }
            self.mail_batch_notified = true;
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
