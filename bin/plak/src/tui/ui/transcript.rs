//! Transcript block folding and approval glue.

use super::{App, DisplayMode, ScrollBlock, TreeNavigator, UserAction, blocks};
use crate::tui::blocks::RowTarget;

impl App {
    /// A restored call still in flight keeps the host's start stamp as its
    /// tip clock, so a tip after `/reload` shows the same elapsed as live.
    pub(super) fn restore_tip_clocks(
        &mut self,
        rows: &[plakous_host_protocol::views::transcript::TranscriptRow],
    ) {
        let now_ms = self.unix_now_ms();
        let now = self.now();
        for tool in rows
            .iter()
            .filter_map(plakous_host_protocol::views::transcript::TranscriptRow::as_tool)
        {
            let Some(started) = tool.started_at_ms else {
                continue;
            };
            if !super::tool_is_running(&tool.state) {
                continue;
            }
            let age = std::time::Duration::from_millis(now_ms.saturating_sub(started));
            if let Some(at) = now.checked_sub(age) {
                self.pane
                    .tool_started
                    .entry(tool.call_id.clone())
                    .or_insert(at);
            }
        }
    }

    /// `Enter`/`Ctrl-O` on a transcript row (spec 02 §5.4): the block's first
    /// toggle — a group summary opens its items, a merged row its calls, an
    /// item, thinking or milestone row its own detail.
    pub(super) fn toggle_tool_expand_at(&mut self, block_idx: usize) {
        let plans = blocks::plan_rows(&self.pane.blocks, self.pane_prefs());
        // A block folded away under a group summary or a collapsed cell is
        // drawn nowhere, so a fold aimed at it would be invisible. The row
        // that stands for it on screen owns the fold: the nearest preceding
        // block that is actually drawn.
        let block_idx = if plans.get(block_idx).is_some_and(|plan| plan.hidden) {
            plans[..block_idx]
                .iter()
                .rposition(|plan| !plan.hidden)
                .unwrap_or(block_idx)
        } else {
            block_idx
        };
        let target = self
            .row_targets(block_idx, plans.get(block_idx))
            .first()
            .copied()
            .unwrap_or(RowTarget::Item);
        self.toggle_row(block_idx, target);
    }

    /// What a click on each of a block's leading one-line rows toggles.
    pub(super) fn row_targets(&self, idx: usize, plan: Option<&blocks::RowPlan>) -> Vec<RowTarget> {
        let (Some(block), Some(plan)) = (self.pane.blocks.get(idx), plan) else {
            return Vec::new();
        };
        if matches!(block, ScrollBlock::Thinking { .. }) && !self.render_prefs.show_thinking {
            return Vec::new();
        }
        blocks::lead_targets(block, plan)
    }

    /// Toggle one row of block `idx`.
    pub(super) fn toggle_row(&mut self, idx: usize, target: RowTarget) {
        let focused = idx;
        let idx = match target {
            RowTarget::Group => blocks::plan_rows(&self.pane.blocks, self.pane_prefs())
                .get(idx)
                .and_then(|plan| plan.group_owner)
                .unwrap_or(idx),
            RowTarget::CellMore(owner)
            | RowTarget::CellOutput(owner)
            | RowTarget::CellHiddenLines(owner) => owner,
            _ => idx,
        };
        let Some(block) = self.pane.blocks.get_mut(idx) else {
            return;
        };
        self.pane.focused_block = Some(if target == RowTarget::Group {
            focused
        } else {
            idx
        });
        match (target, block) {
            (RowTarget::Group, ScrollBlock::Mail { group_open, .. }) => {
                *group_open = !*group_open;
                self.mark_transcript_structural();
            }
            (
                RowTarget::Group,
                ScrollBlock::Tool {
                    group_open, manual, ..
                },
            ) => {
                *group_open = !*group_open;
                *manual = true;
                self.mark_transcript_structural();
            }
            (RowTarget::Merge, ScrollBlock::Tool { view, .. }) => {
                view.members_open = !view.members_open;
                self.mark_transcript_structural();
            }
            (RowTarget::HiddenLines, ScrollBlock::Tool { view, .. }) => {
                view.full_output = true;
                self.mark_block_changed(idx);
            }
            (RowTarget::CellOutput(_), ScrollBlock::Tool { view, .. }) => {
                view.output_open = !view.output_open;
                self.request_item_detail(idx);
                self.mark_block_changed(idx);
            }
            (RowTarget::CellHiddenLines(_), ScrollBlock::Tool { view, .. }) => {
                view.full_output = true;
                self.mark_block_changed(idx);
            }
            (RowTarget::Item | RowTarget::CellMore(_), block) => {
                if matches!(block, ScrollBlock::Thinking { text, sealed, .. } if !*sealed || !blocks::thinking_has_detail(text))
                {
                    return;
                }
                let Some(mode) = block.display_mode() else {
                    return;
                };
                if matches!(target, RowTarget::CellMore(_)) && mode == DisplayMode::Expanded {
                    return;
                }
                let next = if mode == DisplayMode::Expanded {
                    DisplayMode::Collapsed
                } else {
                    DisplayMode::Expanded
                };
                block.set_display_mode(next, true);
                if let ScrollBlock::Tool { view, .. } = block {
                    view.full_output = false;
                    if next == DisplayMode::Collapsed {
                        view.output_open = false;
                        super::call_detail::close_item_view(view);
                    }
                }
                // A cell's fold changes which nested rows are drawn; any
                // other detail is local to its own block and splices.
                let cell = matches!(
                    block,
                    ScrollBlock::Tool {
                        kind: crate::tui::presentation::ActivityKind::Compose,
                        ..
                    }
                );
                self.request_item_detail(idx);
                if cell {
                    self.mark_transcript_structural();
                } else {
                    self.mark_block_changed(idx);
                }
            }
            _ => {}
        }
    }

    /// `Ctrl-Shift-O`: fold everything when anything is open, else open every
    /// group, cell and thinking row (never every item's detail, which would
    /// ask the host for every call); per-row overrides are cleared.
    pub(super) fn toggle_all_blocks(&mut self) {
        let any_open = self.pane.blocks.iter().any(|block| {
            block.display_mode().is_some_and(DisplayMode::shows_detail)
                || matches!(
                    block,
                    ScrollBlock::Mail {
                        group_open: true,
                        ..
                    }
                )
                || matches!(
                    block,
                    ScrollBlock::Tool { group_open, view, .. } if *group_open || view.members_open
                )
        });
        for block in &mut self.pane.blocks {
            block.clear_manual_override();
            let opens = !any_open
                && match block {
                    ScrollBlock::Tool { kind, .. } => {
                        *kind == crate::tui::presentation::ActivityKind::Compose
                    }
                    ScrollBlock::Thinking { text, sealed, .. } => {
                        *sealed && blocks::thinking_has_detail(text)
                    }
                    _ => false,
                };
            if block.display_mode().is_some() {
                let next = if opens {
                    DisplayMode::Expanded
                } else {
                    DisplayMode::Collapsed
                };
                block.set_display_mode(next, false);
            }
            if let ScrollBlock::Mail { group_open, .. } = block {
                *group_open = !any_open;
            }
            if let ScrollBlock::Tool {
                group_open, view, ..
            } = block
            {
                *group_open = !any_open;
                view.members_open = false;
                view.full_output = false;
                view.output_open = false;
            }
        }
        self.mark_transcript_structural();
    }

    /// The row the fold keymap targets: the operator's focus, else the last
    /// foldable block that is actually drawn.
    ///
    /// A child folded under a collapsed Compose (§5.8) or into a §5.5 group head
    /// is not on screen, so folding it would change nothing the operator can
    /// see — a Compose that cannot be expanded with Ctrl-O.
    pub(super) fn focused_foldable_block(&self) -> Option<usize> {
        let plans = blocks::plan_rows(&self.pane.blocks, self.pane_prefs());
        let drawn = |idx: usize| !self.row_targets(idx, plans.get(idx)).is_empty();
        if let Some(idx) = self.pane.focused_block
            && self
                .pane
                .blocks
                .get(idx)
                .is_some_and(ScrollBlock::is_foldable)
            && drawn(idx)
        {
            return Some(idx);
        }
        self.pane
            .blocks
            .iter()
            .enumerate()
            .rev()
            .find(|(idx, block)| block.is_foldable() && drawn(*idx))
            .map(|(idx, _)| idx)
    }

    pub(super) fn toggle_last_tool_expand(&mut self) {
        if let Some(idx) = self.focused_foldable_block() {
            self.toggle_tool_expand_at(idx);
        }
    }

    /// Whether §5.9 `Enter` can open the focused subagent's transcript.
    ///
    /// The transcript row carries a tool call id, never an agent id or a
    /// snapshot revision, so the intent is the existing inert navigator open —
    /// the host remains the authority that resolves the child and its page.
    pub(super) fn focused_agent_transcript_available(&self) -> bool {
        let Some(idx) = self.pane.focused_block else {
            return false;
        };
        if blocks::plan_rows(&self.pane.blocks, self.pane_prefs())
            .get(idx)
            .is_some_and(|plan| {
                plan.hidden || plan.activity_summary.is_some() || plan.group_child.is_some()
            })
        {
            return false;
        }
        // §5.9: only Spawn Agent opens the child view. Wait Agent folds like Direct.
        matches!(
            self.pane.blocks.get(idx),
            Some(ScrollBlock::Tool {
                kind: crate::tui::presentation::ActivityKind::Agent,
                name,
                ..
            }) if name == "spawn_agent"
        ) && self.tree_navigator.is_none()
    }

    pub(super) fn open_focused_agent_transcript(&mut self) -> Option<UserAction> {
        if !self.focused_agent_transcript_available() {
            return None;
        }
        self.activity_tab = Some(super::processes::ActivityTab::Agents);
        self.tree_navigator = Some(TreeNavigator::awaiting());
        self.mark_dirty();
        Some(UserAction::OpenTopologyNavigator)
    }

    /// Open the inert agent canvas, unless it is already open.
    pub(super) fn open_tree_navigator(&mut self) -> Option<UserAction> {
        self.activity_tab = Some(super::processes::ActivityTab::Agents);
        self.activity_last_tab = super::processes::ActivityTab::Agents;
        if self.tree_navigator.is_some() {
            self.mark_dirty();
            return None;
        }
        self.tree_navigator = Some(TreeNavigator::awaiting());
        self.mark_dirty();
        Some(UserAction::OpenTopologyNavigator)
    }

    /// The pending approval row, when the host has one open.
    pub(super) fn pending_approval(&self) -> Option<&str> {
        self.open_approval.as_deref()
    }

    /// Retire an approval the host stopped talking about.
    ///
    /// `SetBusy(false)` without an `ApprovalResolved` means the turn ended
    /// under a lagged channel, an unsettled cancel or a dropped host. The row
    /// must not stay `◆ Waiting…` at idle: it would hold the redraw loop at
    /// the busy cadence for ever and keep `a`/`d` hijacked until `/clear`.
    pub(super) fn retire_unresolved_approval(&mut self) {
        let Some(approval_id) = self.open_approval.take() else {
            return;
        };
        let Some(idx) = self.pane.blocks.iter().rposition(|block| {
            matches!(block, ScrollBlock::Approval { approval_id: id, resolution: None, .. } if *id == approval_id)
        }) else {
            return;
        };
        if let ScrollBlock::Approval { resolution, .. } = &mut self.pane.blocks[idx] {
            *resolution = Some("unresolved · turn ended".to_owned());
        }
        self.mark_block_changed(idx);
    }

    /// `a` / `d` on an open approval row.
    pub(super) fn resolve_pending_approval(
        &mut self,
        choice: crate::tui::ApprovalChoice,
    ) -> Option<UserAction> {
        let approval_id = self.pending_approval()?.to_owned();
        Some(UserAction::ResolveApproval {
            approval_id,
            choice,
        })
    }
}
