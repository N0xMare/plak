//! Changed-key block reconciliation; local chrome is not keyed transcript content.

use plakous_host_protocol::views::transcript::{RowKey, TranscriptRow};

use super::{App, ScrollBlock, tool_is_running};

mod index;
pub(super) use index::BlockIndex;

impl App {
    pub(super) fn local_keyed_blocks(&self) -> Vec<(Option<RowKey>, ScrollBlock)> {
        self.pane.blocks.iter().enumerate().filter_map(|(at, block)| {
            if !self.pane.keyed.is_local(at)
                || matches!(block, ScrollBlock::System(text) if Some(text) == self.opening_note.as_ref()) {
                return None;
            }
            Some((self.pane.keyed.local_anchor(at), block.clone()))
        }).collect()
    }

    pub(super) fn restore_local_keyed_blocks(
        &mut self,
        blocks: &mut Vec<ScrollBlock>,
        locals: Vec<(Option<RowKey>, ScrollBlock)>,
    ) {
        for (key, block) in locals {
            let at = self.pane.keyed.after(key, blocks.len());
            self.pane.keyed.splice(at, 0, 1);
            blocks.insert(at, block);
        }
    }

    pub(super) fn push_live_block(&mut self, block: ScrollBlock) {
        self.push_block(block);
        if !self.pane.blocks.is_empty() {
            self.note_live_block(self.pane.blocks.len() - 1);
        }
    }

    pub(super) fn note_live_block(&mut self, at: usize) {
        if let Some(block) = self.pane.blocks.get(at) {
            self.pane.keyed.note_preview(at, block);
        }
    }

    pub(super) fn keyed_snapshot_blocks(&mut self) -> Vec<ScrollBlock> {
        self.pane.keyed = BlockIndex::default();
        let mut blocks = Vec::new();
        for (key, row) in self.pane.presentation.items() {
            if let Some(mut block) = ScrollBlock::from_row(row.clone()) {
                if key.anchor >= 1_u64 << 63
                    && let ScrollBlock::Thinking {
                        sealed,
                        elapsed_ms: None,
                        ..
                    } = &mut block
                {
                    *sealed = false;
                }
                self.pane.keyed.bind(*key, blocks.len(), &block);
                blocks.push(block);
            }
        }
        blocks
    }

    pub(super) fn patch_keyed_blocks(&mut self, upserts: Vec<RowKey>, deletes: Vec<RowKey>) {
        // Bind replacements first: a settled call can change its key while
        // retaining its block, focus, selection, and following local notices.
        for key in upserts {
            let Some(row) = self.pane.presentation.items().get(&key) else {
                continue;
            };
            #[cfg(test)]
            {
                self.pane.keyed_block_work += 1;
            }
            let Some(mut block) = ScrollBlock::from_row(row.clone()) else {
                if let Some(at) = self.pane.keyed.get(key) {
                    self.remove_keyed_block(at);
                }
                continue;
            };
            if key.anchor >= 1_u64 << 63
                && let ScrollBlock::Thinking {
                    sealed,
                    elapsed_ms: None,
                    ..
                } = &mut block
            {
                *sealed = false;
            }
            let at = self
                .pane
                .keyed
                .get(key)
                .or_else(|| self.pane.keyed.preview(&block))
                .map(|at| self.relocate_keyed_group(at, key));
            let old = if let Some(at) = at {
                Some(std::mem::replace(
                    &mut self.pane.blocks[at],
                    ScrollBlock::System(String::new()),
                ))
            } else {
                None
            };
            if let Some(old) = &old {
                carry_ui(&mut block, old);
            }
            let inserted = at.is_none();
            let at = if let Some(at) = at {
                self.pane.transcript_bytes = self
                    .pane
                    .transcript_bytes
                    .saturating_sub(old.as_ref().map_or(0, ScrollBlock::retained_bytes))
                    .saturating_add(block.retained_bytes());
                self.pane.blocks[at] = block;
                self.mark_block_changed(at);
                at
            } else {
                let at = self.pane.keyed.insertion(key, self.pane.blocks.len());
                self.splice_keyed_indices(at, 0, 1);
                self.pane.transcript_bytes = self
                    .pane
                    .transcript_bytes
                    .saturating_add(block.retained_bytes());
                self.pane.blocks.insert(at, block);
                at
            };
            self.pane.keyed.bind(key, at, &self.pane.blocks[at]);
            self.refresh_changed_clock(at, old.as_ref());
            if inserted {
                self.mark_transcript_structural();
            }
        }
        for key in deletes {
            if let Some(at) = self.pane.keyed.get(key) {
                self.remove_keyed_block(at);
            }
        }
        // A boundary reconciles all preceding droppable previews, including
        // an empty canonical settlement after a failed response.
        for at in self.pane.keyed.pending().into_iter().rev() {
            self.remove_keyed_block(at);
        }
        if let Some(row) = self
            .pane
            .presentation
            .items()
            .last_key_value()
            .map(|(_, row)| row)
            && !matches!(row, TranscriptRow::Assistant { .. })
        {
            self.pane.assistant_stream_active = false;
        }
    }

    fn remove_keyed_block(&mut self, at: usize) -> ScrollBlock {
        let block = self.pane.blocks.remove(at);
        self.pane.transcript_bytes = self
            .pane
            .transcript_bytes
            .saturating_sub(block.retained_bytes());
        self.splice_keyed_indices(at, 1, 0);
        self.mark_transcript_structural();
        block
    }

    fn relocate_keyed_group(&mut self, at: usize, key: RowKey) -> usize {
        if self.pane.keyed.get(key) == Some(at) {
            return at;
        }
        let end = self.pane.keyed.group_end(at, self.pane.blocks.len());
        let target = self.pane.keyed.insertion(key, self.pane.blocks.len());
        if (at..=end).contains(&target) {
            return at;
        }
        let count = end - at;
        let start = if target < at { target } else { target - count };
        let moved: Vec<_> = self.pane.blocks.drain(at..end).collect();
        self.pane.blocks.splice(start..start, moved);
        let remap = |index: usize| {
            Some(if (at..end).contains(&index) {
                start + index - at
            } else if target < at && (target..at).contains(&index) {
                index + count
            } else if target > end && (end..target).contains(&index) {
                index - count
            } else {
                index
            })
        };
        self.pane
            .keyed
            .remap(at.min(target)..end.max(target), remap);
        self.remap_keyed_ui(remap);
        self.mark_transcript_structural();
        start
    }

    fn splice_keyed_indices(&mut self, at: usize, removed: usize, added: usize) {
        self.pane.keyed.splice(at, removed, added);
        let remap = |index: usize| {
            if index < at {
                Some(index)
            } else if index < at + removed {
                None
            } else {
                Some(index - removed + added)
            }
        };
        self.remap_keyed_ui(remap);
    }

    fn remap_keyed_ui(&mut self, remap: impl Fn(usize) -> Option<usize> + Copy) {
        self.pane.focused_block = self.pane.focused_block.and_then(remap);
        self.pane.transcript_view_anchor = self.pane.transcript_view_anchor.map(|(index, row)| {
            remap(index).map_or((index.saturating_sub(1), 0), |index| (index, row))
        });
        self.selection.remap_blocks(remap);
        self.pending_tool_click = None;
    }

    fn refresh_changed_clock(&mut self, at: usize, old: Option<&ScrollBlock>) {
        let now = self.now();
        match &self.pane.blocks[at] {
            ScrollBlock::Thinking {
                location_key,
                sealed,
                ..
            } => {
                if *sealed {
                    self.pane.thinking_started.remove(location_key);
                } else {
                    self.pane
                        .thinking_started
                        .entry(location_key.clone())
                        .or_insert(now);
                }
            }
            ScrollBlock::Tool {
                call_id: Some(id),
                state,
                started_at_ms,
                ..
            } => {
                let id = id.clone();
                if tool_is_running(state) {
                    let start = started_at_ms
                        .and_then(|start| {
                            now.checked_sub(std::time::Duration::from_millis(
                                self.unix_now_ms().saturating_sub(start),
                            ))
                        })
                        .unwrap_or(now);
                    self.pane.tool_started.entry(id).or_insert(start);
                } else {
                    self.pane.tool_started.remove(&id);
                    if matches!(old, Some(ScrollBlock::Tool { state, .. }) if tool_is_running(state))
                    {
                        if let Some(key) = self.detail_key(&id) {
                            self.call_details.note_ended(key);
                        }
                        self.request_item_detail(at);
                    }
                }
            }
            _ => {}
        }
    }
}

fn carry_ui(block: &mut ScrollBlock, old: &ScrollBlock) {
    match (block, old) {
        (
            ScrollBlock::Tool {
                mode,
                manual,
                group_open,
                view,
                ..
            },
            ScrollBlock::Tool {
                mode: old_mode,
                manual: old_manual,
                group_open: old_group,
                view: old_view,
                ..
            },
        ) => {
            *mode = *old_mode;
            *manual = *old_manual;
            *group_open = *old_group;
            view.clone_from(old_view);
        }
        (
            ScrollBlock::Thinking { mode, manual, .. },
            ScrollBlock::Thinking {
                mode: old_mode,
                manual: true,
                ..
            },
        ) => {
            *mode = *old_mode;
            *manual = true;
        }
        (
            ScrollBlock::Mail {
                open, group_open, ..
            },
            ScrollBlock::Mail {
                open: old_open,
                group_open: old_group,
                ..
            },
        ) => {
            *open = *old_open;
            *group_open = *old_group;
        }
        (ScrollBlock::Milestone { open, .. }, ScrollBlock::Milestone { open: old_open, .. }) => {
            *open = *old_open;
        }
        _ => {}
    }
}
