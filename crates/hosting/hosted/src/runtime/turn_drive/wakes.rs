//! Root observations and deadlines that do not belong to the turn's event stream.

use super::{AgentEventKind, Instant, RootTurn, RootWake, TurnHost};
use crate::runtime::child_live::{ChildObservers, ObservedChild};
use crate::runtime::projection::live::{Applied, LiveProjector};
use crate::topology_runtime::ProductTopologyRuntime;
use std::sync::Arc;
use tokio::sync::{Notify, watch};

pub(super) struct Housekeeping {
    pub(super) tree: Option<watch::Receiver<()>>,
    pub(super) resync_children: Option<Arc<ProductTopologyRuntime>>,
    tree_due: TreeCheck,
    pub(super) tree_output_deadline: Option<tokio::time::Instant>,
    mail_due: bool,
    pub(super) jobs: Arc<Notify>,
    pub(super) projection: Option<watch::Receiver<Applied>>,
    queue_due: bool,
    pub(super) usage_not_before: Instant,
    pub(super) mailbox_closed: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum TreeCheck {
    #[default]
    None,
    Progress,
    Topology,
}

impl Housekeeping {
    pub(super) fn new<P: plakous_host_protocol::ProtocolExtension>(host: &TurnHost<'_, P>) -> Self {
        Self {
            tree: None,
            resync_children: None,
            tree_due: TreeCheck::Topology,
            tree_output_deadline: None,
            mail_due: true,
            jobs: host.roots.job_registry().observations_ready(),
            projection: host
                .live_projector
                .as_deref()
                .map(LiveProjector::observe_applied),
            queue_due: true,
            usage_not_before: Instant::now(),
            mailbox_closed: false,
        }
    }
}

pub(super) async fn changed<T>(receiver: Option<&mut watch::Receiver<T>>) -> bool {
    if let Some(receiver) = receiver {
        receiver.changed().await.is_ok()
    } else {
        std::future::pending().await
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> RootTurn<'_, P> {
    pub(super) fn runtime_housekeeping(&mut self, event: &plakous::agent::AgentEvent) {
        // A model/tool boundary can re-open notice delivery at a new
        // checkpoint or begin a repeat process observation.
        if matches!(
            event.kind(),
            AgentEventKind::ModelDispatchStarted
                | AgentEventKind::ToolStarted { .. }
                | AgentEventKind::ToolProgress { .. }
                | AgentEventKind::ToolCompleted { .. }
        ) {
            self.housekeeping.mail_due = true;
            if self.housekeeping.tree.is_none()
                || self.host.last_tree_revision.last_notice_revision
                    != self.host.last_tree_revision.last_revision
            {
                self.housekeeping.tree_due = self.housekeeping.tree_due.max(TreeCheck::Progress);
            }
        }
        if self.housekeeping.projection.is_none() && event.feed_seq().is_some() {
            self.housekeeping.queue_due = true;
        }
    }

    pub(super) fn serve_work(&mut self, wake: RootWake) {
        let state = &mut self.housekeeping;
        match wake {
            RootWake::Intent | RootWake::Deferred => {
                #[cfg(test)]
                if matches!(wake, RootWake::Intent) {
                    self.last_wake = "intent";
                }
                if let Some(live) = self.host.live_projector.as_mut() {
                    live.tick(self.host.live_target.clone());
                }
                state.queue_due = true;
            }
            RootWake::Tree(open) => {
                if !open {
                    state.tree = None;
                }
                state.tree_due = TreeCheck::Topology;
                state.mail_due = true;
            }
            RootWake::ChildProgress => {
                state.tree_due = state.tree_due.max(TreeCheck::Progress);
                state.tree_output_deadline = None;
            }
            RootWake::ChildTable(children) => {
                if let Some(tree) = state.resync_children.take() {
                    self.host
                        .last_tree_revision
                        .child_live
                        .sync_observations(&tree, children);
                }
                state.tree_due = state.tree_due.max(TreeCheck::Progress);
            }
            RootWake::NoticeReply => {
                self.notice_reply = None;
                state.mail_due = true;
            }
            RootWake::Projection(open) => {
                if !open {
                    state.projection = None;
                }
                state.queue_due = true;
            }
            RootWake::Queue(commands) => self.finish_queue_projection(commands),
            RootWake::Flush => {
                #[cfg(test)]
                {
                    self.last_wake = "flush";
                }
                super::flush_assistant_delta(
                    &mut self.delta_buf,
                    &mut self.last_flush,
                    self.host.events,
                    true,
                );
            }
            RootWake::Usage => {
                self.host.subscription_usage_refresh.poll_while_watched(
                    self.host.subscription_service.clone(),
                    self.host.session.account_ref().ok(),
                );
                state.usage_not_before = Instant::now() + std::time::Duration::from_secs(1);
            }
            RootWake::Notice | RootWake::Jobs | RootWake::StepControl | RootWake::Cancel => {}
        }
    }

    pub(super) fn observe_housekeeping(&mut self) {
        let state = &mut self.housekeeping;
        if state.queue_due && self.queue_refresh.is_none() {
            state.queue_due = false;
            self.start_queue_projection();
        }
        let due = std::mem::take(&mut self.housekeeping.tree_due);
        if due != TreeCheck::None {
            let push = &mut *self.host.last_tree_revision;
            if let Some(tree) = push.runtime(self.host.rows, self.host.session) {
                self.housekeeping
                    .tree
                    .get_or_insert_with(|| tree.observe_changes());
                if due == TreeCheck::Topology {
                    // A concurrent child admission owns this table until it
                    // publishes. Wait for that lock in select, not inline
                    // ahead of operator intents or the canonical terminal.
                    self.housekeeping.resync_children =
                        (!push.child_live.sync(&tree)).then_some(tree);
                }
            }
            super::emit_tree_snapshot_if_revision_advanced(
                self.host.rows,
                self.host.store,
                self.host.home,
                self.host.session,
                self.host.last_tree_revision,
                self.host.events,
                false,
            );
            let push = &self.host.last_tree_revision;
            let now = tokio::time::Instant::now();
            // Only pending output arms a tree deadline: coalesced progress
            // or notices from a revision that could not settle.
            let progress = push.next_progress_snapshot.filter(|at| {
                (!push.notices_pending || *at > now) && push.child_live.material_progress_changed()
            });
            let notices = push
                .notices_pending
                .then(|| now + std::time::Duration::from_millis(250));
            self.housekeeping.tree_output_deadline = progress.into_iter().chain(notices).min();
        }
        if std::mem::take(&mut self.housekeeping.mail_due) {
            self.poll_mail_notice();
        }
    }
}

pub(super) async fn wait_children(tree: Option<&ProductTopologyRuntime>) -> Vec<ObservedChild> {
    if let Some(tree) = tree {
        ChildObservers::read_live(tree).await
    } else {
        std::future::pending().await
    }
}

pub(super) async fn wait_reply<T>(reply: Option<&mut tokio::sync::oneshot::Receiver<T>>) {
    if let Some(reply) = reply {
        let _ = reply.await;
    } else {
        std::future::pending::<()>().await;
    }
}

pub(super) async fn wait_queue(
    refresh: Option<&mut super::queue_projection::QueueRefresh>,
) -> Option<super::sessions::QueueCommands> {
    if let Some(refresh) = refresh {
        (&mut refresh.task).await.ok()?.ok()
    } else {
        std::future::pending().await
    }
}
