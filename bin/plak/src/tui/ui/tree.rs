//! Topology navigator state, keys, and mouse.

use super::terminal::copy_text_osc52;
use super::{
    App, CanvasTree, KeyCode, MouseEvent, MouseEventKind, RESTORED_PAUSED_NAVIGATOR_HINT,
    TopologyNavigatorKey, TopologyNavigatorOutcome, UserAction, event, route_submit,
    tree_navigator_key,
};

/// The product's acknowledged occurrences for the tree the TUI last saw.
#[derive(Debug, Default)]
pub(super) struct AgentAcknowledgements {
    /// Tree the set belongs to.
    tree_id: Option<String>,
    pub(super) attempts: std::collections::BTreeSet<crate::tui::AgentAcknowledgement>,
    /// Tree whose set still needs asking for.
    request: Option<String>,
    /// Tree last asked for, so snapshots in flight do not ask again.
    asked: Option<String>,
    /// Tree whose last read failed. The next snapshot of it asks again;
    /// asking from the failure itself would spin while the store stays down.
    retry: Option<String>,
}

impl App {
    /// The Agents tab is showing a list.
    pub(super) fn agents_list_open(&self) -> bool {
        self.activity_tab == Some(super::processes::ActivityTab::Agents)
            && self
                .tree_navigator
                .as_ref()
                .is_some_and(|nav| nav.snapshot.is_some())
    }

    /// Wall clock the Agents list evaluates ages at.
    pub(super) fn agents_now(&self) -> u64 {
        self.agents_clock.unwrap_or_else(super::unix_now)
    }

    /// Ask the product for the acknowledged attempts of a newly seen tree.
    pub(crate) fn maybe_request_agent_acknowledgements(
        &mut self,
        actions: Option<&crate::tui::UserActionSender>,
    ) {
        let Some(tree_id) = self.agent_acks.request.clone() else {
            return;
        };
        if self.agent_acks.asked.as_ref() == Some(&tree_id) {
            self.agent_acks.request = None;
            return;
        }
        if let Some(sender) = actions
            && sender
                .send(UserAction::Extension(
                    crate::tui::SurfaceIntent::ListAgentAcknowledgements {
                        tree_id: tree_id.clone(),
                    },
                ))
                .is_ok()
        {
            self.agent_acks.request = None;
            self.agent_acks.asked = Some(tree_id);
        }
    }

    /// The product could not read this tree's acknowledgements. Clear the
    /// in-flight ask and remember the tree, so the next snapshot requests
    /// again. An empty set is not applied: failure is not "nothing is
    /// acknowledged", and the failure itself does not ask (that would spin).
    pub(super) fn agent_acknowledgements_unavailable(&mut self, tree_id: &str) {
        if self.agent_acks.asked.as_deref() == Some(tree_id) {
            self.agent_acks.asked = None;
            self.agent_acks.retry = Some(tree_id.to_owned());
        }
    }

    /// `R` asks again even when this tree's set was already fetched.
    pub(super) fn rearm_agent_acknowledgements(&mut self) {
        let tree_id = self
            .tree_navigator
            .as_ref()
            .and_then(|nav| nav.snapshot.as_ref())
            .map(|snap| snap.tree_id.clone());
        if let Some(tree_id) = tree_id {
            self.agent_acks.asked = None;
            self.agent_acks.retry = None;
            self.agent_acks.request = Some(tree_id);
        }
    }

    /// The product's complete acknowledged set for one tree.
    pub(super) fn apply_agent_acknowledgements(
        &mut self,
        tree_id: String,
        attempts: Vec<crate::tui::AgentAcknowledgement>,
    ) {
        let attempts: std::collections::BTreeSet<_> = attempts.into_iter().collect();
        if let Some(nav) = self.tree_navigator.as_mut() {
            nav.set_acknowledged(&tree_id, &attempts);
        }
        if self.agent_acks.retry.as_deref() == Some(tree_id.as_str()) {
            self.agent_acks.retry = None;
        }
        self.agent_acks.tree_id = Some(tree_id);
        self.agent_acks.attempts = attempts;
        self.mark_dirty();
    }

    /// The root has no assignment, so the tree carries no turn state for it;
    /// the canvas paints the header's phase instead (the root box read
    /// `running` on an idle session).
    pub(super) fn sync_root_turn_projection(&mut self) {
        let active = matches!(
            self.phase.as_str(),
            "running" | "interrupting" | "compacting"
        );
        if let Some(nav) = self.tree_navigator.as_mut()
            && nav.project_root_turn(active)
        {
            self.mark_dirty();
        }
    }

    pub(super) fn apply_topology_navigator_snapshot(&mut self, snap: CanvasTree) {
        if self.agents_unavailable.take().is_some() {
            self.mark_dirty();
        }
        if self.tree_restored_paused != snap.restored_paused {
            self.tree_restored_paused = snap.restored_paused;
            self.mark_dirty();
        }
        if snap.restored_paused && self.tree_navigator.is_none() {
            self.status_detail = Some(RESTORED_PAUSED_NAVIGATOR_HINT.to_owned());
            self.mark_dirty();
        }
        let unseen = self.agent_acks.tree_id.as_deref() != Some(snap.tree_id.as_str());
        let retry = self.agent_acks.retry.as_deref() == Some(snap.tree_id.as_str());
        if unseen || retry {
            self.agent_acks.request = Some(snap.tree_id.clone());
            if retry {
                self.agent_acks.retry = None;
            }
        }
        if self
            .agent_tree
            .as_ref()
            .is_none_or(|kept| kept.tree_id != snap.tree_id || kept.revision <= snap.revision)
        {
            self.agent_tree = Some(snap.clone());
        }
        if let Some(nav) = self.tree_navigator.as_mut() {
            let tree_id = snap.tree_id.clone();
            nav.apply_snapshot(snap);
            if self.agent_acks.tree_id.as_deref() == Some(tree_id.as_str()) {
                nav.set_acknowledged(&tree_id, &self.agent_acks.attempts);
            }
            self.slash_open = false;
            self.mark_dirty();
            self.sync_root_turn_projection();
        }
    }

    /// After attach or in-process `/resume` load, request a tree snapshot.
    ///
    /// Never `ResumeTopologyTree`: the topology contract
    /// say a `RestoredPaused` tree never auto-resumes on reattach, and the
    /// navigator's `r` is its only exit. Overlay stays closed; a crash-restored
    /// queue drain is not resumed either.
    pub(crate) fn maybe_request_tree_snapshot_after_attach(
        &mut self,
        actions: Option<&crate::tui::UserActionSender>,
    ) {
        if !self.pending_open_tree {
            return;
        }
        self.pending_open_tree = false;
        if let Some(tx) = actions {
            let _ = tx.send(UserAction::OpenTopologyNavigator);
        }
    }

    /// After attach, send a prompt that was queued while the agent was preparing.
    pub(crate) fn maybe_flush_pending_prompt_after_attach(
        &mut self,
        actions: Option<&crate::tui::UserActionSender>,
    ) {
        if !self.agent_enabled {
            return;
        }
        let Some(text) = self.pending_prompt.take() else {
            return;
        };
        self.clear_input();
        self.slash_open = false;
        self.queue_focused = false;
        self.history_browse = None;
        self.mark_dirty();
        if let Some(tx) = actions {
            let _ = tx.send(route_submit(self.busy, false, text));
        }
    }

    pub(super) fn handle_tree_navigator_key(&mut self, code: KeyCode) -> Option<UserAction> {
        if self.tree_navigator.is_none() {
            if code == KeyCode::Esc {
                self.close_activity();
            }
            return None;
        }
        let nav_key = tree_navigator_key(code)?;
        if matches!(nav_key, TopologyNavigatorKey::Refresh) {
            self.rearm_agent_acknowledgements();
        }
        let copy = matches!(nav_key, TopologyNavigatorKey::Copy);
        let nav = self.tree_navigator.as_mut()?;
        let outcome = nav.handle_key(nav_key);
        if copy && matches!(outcome, TopologyNavigatorOutcome::Consumed) {
            let text = nav.copy_text();
            if !text.is_empty() {
                copy_text_osc52(&text);
            }
        }
        self.mark_dirty();
        match outcome {
            TopologyNavigatorOutcome::Dismiss => {
                self.close_activity();
                None
            }
            TopologyNavigatorOutcome::Consumed => None,
            TopologyNavigatorOutcome::Action(action) => Some(action),
            TopologyNavigatorOutcome::Confirm(action) => self.confirm_destructive(action),
            TopologyNavigatorOutcome::ShowRoot => {
                self.close_activity();
                None
            }
            TopologyNavigatorOutcome::ShowAgent { agent_id, label } => {
                // The navigator stays, so Esc returns to this row.
                self.open_agent_view(agent_id, label);
                None
            }
        }
    }

    pub(super) fn handle_tree_navigator_mouse(&mut self, mouse: MouseEvent) -> Option<UserAction> {
        let point = (mouse.column, mouse.row).into();
        match mouse.kind {
            MouseEventKind::ScrollUp
                if self.tree_popup.is_some_and(|popup| popup.contains(point)) =>
            {
                if let Some(nav) = self.tree_navigator.as_mut() {
                    nav.move_selection(-1);
                }
                self.mark_dirty();
            }
            MouseEventKind::ScrollDown
                if self.tree_popup.is_some_and(|popup| popup.contains(point)) =>
            {
                if let Some(nav) = self.tree_navigator.as_mut() {
                    nav.move_selection(1);
                }
                self.mark_dirty();
            }
            MouseEventKind::Down(event::MouseButton::Left) => {
                if self.tree_popup.is_some_and(|popup| !popup.contains(point)) {
                    self.close_activity();
                    return None;
                }
                if let Some(agent_id) = self
                    .tree_hits
                    .iter()
                    .find(|(_, rect)| rect.contains(point))
                    .map(|(agent_id, _)| agent_id.clone())
                {
                    let double = self.list_clicks.click(
                        super::picker::ClickList::Agents,
                        &agent_id,
                        super::Instant::now(),
                    );
                    if let Some(nav) = self.tree_navigator.as_mut() {
                        nav.focus_on(&agent_id);
                    }
                    self.mark_dirty();
                    if double {
                        return self.handle_tree_navigator_key(KeyCode::Enter);
                    }
                }
            }
            _ => {}
        }
        None
    }
}
