//! Key-owned UI restoration; local blocks move across resync without clones.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use plakous_host_protocol::views::transcript::RowKey;

use super::{App, ScrollBlock, carry_ui, tool_is_running};

pub(in crate::tui::ui) struct KeyedRestore {
    old: Vec<ScrollBlock>,
    keys: Vec<Option<RowKey>>,
    mapped: Vec<Option<usize>>,
    unchanged: Vec<bool>,
    locals: Vec<LocalBlock>,
    notes: Vec<(usize, ScrollBlock)>,
    running: BTreeSet<String>,
    same_session: bool,
}

struct LocalBlock {
    old_at: usize,
    anchor: Option<RowKey>,
    block: ScrollBlock,
}

impl App {
    pub(in crate::tui::ui) fn take_keyed_restore(
        &mut self,
        keep_local: bool,
        same_session: bool,
        root: bool,
    ) -> KeyedRestore {
        if same_session {
            self.pane
                .selection
                .prepare_keyed_restore(self.pane.transcript_cache.layout());
        } else {
            self.pane.selection.clear();
        }
        let mut old = std::mem::take(&mut self.pane.blocks);
        let keys = (0..old.len())
            .map(|at| self.pane.keyed.key_at(at))
            .collect();
        let running = old
            .iter()
            .filter_map(|block| match block {
                ScrollBlock::Tool {
                    call_id: Some(id),
                    state,
                    ..
                } if same_session && tool_is_running(state) => Some(id.clone()),
                _ => None,
            })
            .collect();
        let notes_from = root.then(|| self.open_notes_from.take()).flatten();
        let mut locals = Vec::new();
        let mut notes = Vec::new();
        for (at, block) in old.iter_mut().enumerate() {
            if keep_local
                && self.pane.keyed.is_local(at)
                && !matches!(&*block, ScrollBlock::System(text) if Some(text) == self.opening_note.as_ref())
            {
                locals.push(LocalBlock {
                    old_at: at,
                    anchor: self.pane.keyed.local_anchor(at),
                    block: std::mem::replace(block, ScrollBlock::System(String::new())),
                });
            } else if notes_from.is_some_and(|from| at >= from)
                && matches!(block, ScrollBlock::System(_) | ScrollBlock::Error(_))
            {
                notes.push((
                    at,
                    std::mem::replace(block, ScrollBlock::System(String::new())),
                ));
            }
        }
        KeyedRestore {
            mapped: vec![None; old.len()],
            unchanged: vec![false; old.len()],
            old,
            keys,
            locals,
            notes,
            running,
            same_session,
        }
    }

    pub(in crate::tui::ui) fn restore_local_keyed_blocks(
        &mut self,
        blocks: &mut Vec<ScrollBlock>,
        restore: &mut KeyedRestore,
    ) {
        let mut floor = 0;
        let mut original = Vec::new();
        for local in restore.locals.drain(..) {
            let at = self
                .pane
                .keyed
                .after(local.anchor, blocks.len())
                .max(floor)
                .min(blocks.len());
            self.pane.keyed.splice(at, 0, 1);
            blocks.insert(at, local.block);
            original.push(local.old_at);
            floor = at + 1;
        }
        let positions: Vec<_> = blocks
            .iter()
            .enumerate()
            .filter(|(at, block)| {
                self.pane.keyed.is_local(*at)
                    && !matches!(block, ScrollBlock::System(text) if Some(text) == self.opening_note.as_ref())
            })
            .map(|(at, _)| at)
            .collect();
        for (old, new) in original.into_iter().zip(positions) {
            restore.mapped[old] = Some(new);
            restore.unchanged[old] = true;
        }
    }

    pub(in crate::tui::ui) fn restore_open_notes(
        blocks: &mut Vec<ScrollBlock>,
        restore: &mut KeyedRestore,
    ) {
        for (old, note) in restore.notes.drain(..) {
            let at = blocks.iter().position(|block| match (block, &note) {
                (ScrollBlock::System(have), ScrollBlock::System(want))
                | (ScrollBlock::Error(have), ScrollBlock::Error(want)) => have == want,
                _ => false,
            });
            let at = at.unwrap_or_else(|| {
                let at = blocks.len();
                blocks.push(note);
                at
            });
            restore.mapped[old] = Some(at);
            restore.unchanged[old] = true;
        }
    }

    pub(in crate::tui::ui) fn finish_keyed_restore(
        &mut self,
        mut restore: KeyedRestore,
    ) -> BTreeSet<String> {
        if restore.same_session {
            let by_key: BTreeMap<_, _> = restore
                .keys
                .iter()
                .enumerate()
                .filter_map(|(at, key)| (*key).map(|key| (key, at)))
                .collect();
            let by_identity: HashMap<_, _> = restore
                .old
                .iter()
                .enumerate()
                .filter_map(|(at, block)| item_identity(block).map(|id| (id, at)))
                .collect();
            let mut provisional_thinking = HashMap::new();
            for (at, block) in restore.old.iter().enumerate() {
                if matches!(block, ScrollBlock::Thinking { .. })
                    && restore.keys[at].is_none_or(|key| key.anchor >= 1_u64 << 63)
                {
                    provisional_thinking
                        .entry(block.body())
                        .and_modify(|index| *index = None)
                        .or_insert(Some(at));
                }
            }
            for (at, block) in self.pane.blocks.iter_mut().enumerate() {
                let Some(key) = self.pane.keyed.key_at(at) else {
                    continue;
                };
                let prior = by_key
                    .get(&key)
                    .copied()
                    .or_else(|| item_identity(block).and_then(|id| by_identity.get(&id).copied()))
                    .or_else(|| {
                        matches!(block, ScrollBlock::Thinking { .. })
                            .then(|| provisional_thinking.get(block.body()).copied().flatten())
                            .flatten()
                    });
                if let Some(old) = prior {
                    carry_ui(block, &restore.old[old]);
                    if restore.mapped[old] != Some(at) || !restore.unchanged[old] {
                        restore.unchanged[old] = *block == restore.old[old];
                    }
                    restore.mapped[old] = Some(at);
                }
            }
        }
        let remap = |at: usize| restore.mapped.get(at).copied().flatten();
        self.pane.focused_block = self.pane.focused_block.and_then(remap);
        self.pane.transcript_view_anchor =
            self.pane.transcript_view_anchor.and_then(|(at, row)| {
                remap(at)
                    .map(|next| (next, row))
                    .or_else(|| {
                        (1..restore.old.len()).find_map(|distance| {
                            at.checked_add(distance)
                                .and_then(remap)
                                .or_else(|| at.checked_sub(distance).and_then(remap))
                                .map(|next| (next, 0))
                        })
                    })
                    .or_else(|| (!self.pane.blocks.is_empty()).then_some((0, 0)))
            });
        self.pane.selection.remap_blocks(|at| {
            restore
                .unchanged
                .get(at)
                .copied()
                .unwrap_or(false)
                .then(|| remap(at))
                .flatten()
        });
        self.pane.pending_tool_click = None;
        restore.running
    }
}

fn item_identity(block: &ScrollBlock) -> Option<(bool, &str)> {
    match block {
        ScrollBlock::Tool {
            call_id: Some(id), ..
        } => Some((true, id.as_str())),
        ScrollBlock::Thinking { location_key, .. } => Some((false, location_key.as_str())),
        _ => None,
    }
}
