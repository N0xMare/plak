//! Refresh queue authority without waiting for a busy turn to end.

use super::{Duration, HostEvent, Instant, TurnDrive, emit_queue_snapshot, sessions};

pub(super) struct QueueRefresh {
    expected: sessions::QueueCommands,
    pub(super) task: tokio::task::JoinHandle<Result<sessions::QueueCommands, String>>,
}

impl<P: plakous_host_protocol::ProtocolExtension> TurnDrive<'_, P> {
    pub(super) async fn publish_applied_steering(&mut self) {
        // SteeringApplied is an observation emitted before its commit. The
        // following prepared request is past that commit and ahead of new
        // model output, so settle the whole canonical prefix at this boundary.
        super::flush_assistant_delta(
            &mut self.delta_buf,
            &mut self.last_flush,
            self.host.events,
            true,
        );
        let target = self.host.live_target.clone();
        let pass = async {
            match self.host.live_projector.as_mut() {
                Some(live) => live.settle(target).await,
                None => target.views(None).await.map(|projected| projected.views),
            }
        };
        let Ok(Ok(views)) =
            tokio::time::timeout(super::super::host_loop::FINAL_PROJECTION_BOUND, pass).await
        else {
            return;
        };
        let Ok(intents) = self.host.store.load_intents(&self.host.session.id) else {
            return;
        };
        let mut transcript = sessions::ProductTranscript::stored(views.transcript(&intents));
        for notice in self.host.session.transcript.pending_notice_items() {
            transcript.record_notice_item(notice);
        }
        let mut items = transcript.items();
        items.insert(
            0,
            plakous_host_protocol::views::transcript::TranscriptItem {
                key: sessions::projector::RowKey::TRUNCATION,
                row: super::super::logging::session_header(self.host.session),
            },
        );
        if self
            .host
            .events
            .publish_transcript(super::super::logging::project_keyed_snapshot(items))
        {
            self.steering_projection_due = false;
        }
    }

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

    /// Applies the root's feed to the in-process tree while the turn runs,
    /// once per model round, inline so tree saves stay ordered with the
    /// tree's own writes and follow the feed deterministically. The round
    /// starts after the previous round's tool results committed, so a wait
    /// that history now holds retires its receipt (spec 03 §4.4). The pass
    /// costs one feed read past the cursor and, for a round that committed
    /// anything, one tree save, concurrent with the model stream; it never
    /// refolds (see [`TreePass::RunningTurn`]). A refused or deferred pass is
    /// projection debt the next round or the next prompt's admission pass
    /// applies, logged once per distinct cause in a turn.
    ///
    /// [`TreePass::RunningTurn`]: super::super::projection::TreePass::RunningTurn
    pub(super) async fn project_running_root_tree(&mut self) {
        if self.host.session.store_revision == 0 {
            return;
        }
        let target = &self.host.live_target;
        let debt = match super::super::projection::project_tree_as(
            &target.sessions,
            self.host.rows,
            &target.tree_id,
            target.followed.session_id(),
            super::super::projection::TreePass::RunningTurn,
        )
        .await
        {
            Ok(super::super::projection::TreePassOutcome::Deferred(reason)) => reason,
            Ok(_) => return,
            Err(error) => String::from(error),
        };
        if self.tree_debt_logged.as_deref() == Some(debt.as_str()) {
            return;
        }
        super::super::logging::log_event(
            self.host.log,
            &self.host.session.id,
            crate::session_log::SessionEvent::new("projector.deferred").str("error", &debt),
        );
        self.tree_debt_logged = Some(debt);
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
