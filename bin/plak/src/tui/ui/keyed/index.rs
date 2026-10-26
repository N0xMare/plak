use std::collections::{BTreeMap, HashMap};
use std::mem::{Discriminant, discriminant};

use plakous_host_protocol::views::transcript::RowKey;

use super::ScrollBlock;

#[derive(Clone, Eq, Hash, PartialEq)]
enum Identity {
    Tool(String),
    Thinking(String),
}

fn identity(block: &ScrollBlock) -> Option<Identity> {
    match block {
        ScrollBlock::Tool {
            call_id: Some(id), ..
        } => Some(Identity::Tool(id.clone())),
        ScrollBlock::Thinking { location_key, .. } => {
            Some(Identity::Thinking(location_key.clone()))
        }
        _ => None,
    }
}

#[derive(Default)]
pub(in crate::tui::ui) struct BlockIndex {
    keys: BTreeMap<RowKey, usize>,
    slots: HashMap<usize, RowKey>,
    identities: HashMap<Identity, usize>,
    previews: Vec<(Discriminant<ScrollBlock>, usize)>,
}

impl BlockIndex {
    pub(in crate::tui::ui) fn is_local(&self, at: usize) -> bool {
        !self.slots.contains_key(&at) && !self.previews.iter().any(|(_, index)| *index == at)
    }

    pub(in crate::tui::ui) fn local_anchor(&self, at: usize) -> Option<RowKey> {
        self.slots
            .iter()
            .filter(|(index, _)| **index < at)
            .max_by_key(|(index, _)| **index)
            .map(|(_, key)| *key)
    }

    pub(in crate::tui::ui) fn after(&self, key: Option<RowKey>, end: usize) -> usize {
        key.map_or(0, |key| {
            self.keys
                .range((std::ops::Bound::Excluded(key), std::ops::Bound::Unbounded))
                .next()
                .map_or(end, |(_, at)| *at)
        })
    }

    pub(in crate::tui::ui) fn get(&self, key: RowKey) -> Option<usize> {
        self.keys.get(&key).copied()
    }

    pub(in crate::tui::ui) fn preview(&self, block: &ScrollBlock) -> Option<usize> {
        identity(block)
            .and_then(|id| self.identities.get(&id).copied())
            .or_else(|| {
                self.previews
                    .iter()
                    .find(|(kind, _)| *kind == discriminant(block))
                    .map(|(_, at)| *at)
            })
    }

    pub(in crate::tui::ui) fn insertion(&self, key: RowKey, end: usize) -> usize {
        self.keys.range(key..).next().map_or(end, |(_, at)| *at)
    }

    pub(in crate::tui::ui) fn group_end(&self, at: usize, end: usize) -> usize {
        self.slots
            .keys()
            .copied()
            .chain(self.previews.iter().map(|(_, index)| *index))
            .filter(|index| *index > at)
            .min()
            .unwrap_or(end)
    }

    pub(in crate::tui::ui) fn bind(&mut self, key: RowKey, at: usize, block: &ScrollBlock) {
        if let Some(old) = self.slots.insert(at, key) {
            self.keys.remove(&old);
        }
        self.keys.insert(key, at);
        self.previews.retain(|(_, index)| *index != at);
        self.remember_identity(at, block);
    }

    fn remember_identity(&mut self, at: usize, block: &ScrollBlock) {
        if let Some(id) = identity(block) {
            self.identities.insert(id, at);
        }
    }

    pub(in crate::tui::ui) fn note_preview(&mut self, at: usize, block: &ScrollBlock) {
        if !self.slots.contains_key(&at) && !self.previews.iter().any(|(_, index)| *index == at) {
            self.previews.push((discriminant(block), at));
        }
        self.remember_identity(at, block);
    }

    pub(in crate::tui::ui) fn pending(&self) -> Vec<usize> {
        let mut indices: Vec<_> = self.previews.iter().map(|(_, at)| *at).collect();
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    pub(in crate::tui::ui) fn splice(&mut self, at: usize, removed: usize, added: usize) {
        let remap = |index: usize| {
            if index < at {
                Some(index)
            } else if index < at + removed {
                None
            } else {
                Some(index - removed + added)
            }
        };
        self.remap(remap);
    }

    pub(in crate::tui::ui) fn remap(&mut self, remap: impl Fn(usize) -> Option<usize>) {
        self.keys.retain(|_, index| match remap(*index) {
            Some(next) => {
                *index = next;
                true
            }
            None => false,
        });
        self.slots = self
            .keys
            .iter()
            .map(|(key, index)| (*index, *key))
            .collect();
        self.identities.retain(|_, index| match remap(*index) {
            Some(next) => {
                *index = next;
                true
            }
            None => false,
        });
        self.previews.retain_mut(|(_, index)| match remap(*index) {
            Some(next) => {
                *index = next;
                true
            }
            None => false,
        });
    }
}
