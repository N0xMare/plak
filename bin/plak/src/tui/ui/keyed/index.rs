use std::collections::{BTreeMap, HashMap};
use std::mem::{Discriminant, discriminant};
use std::ops::{Bound, Range};

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
    head: usize,
    keys: BTreeMap<RowKey, usize>,
    slots: BTreeMap<usize, RowKey>,
    identities: HashMap<Identity, usize>,
    identity_slots: BTreeMap<usize, Identity>,
    previews: BTreeMap<usize, Discriminant<ScrollBlock>>,
    #[cfg(test)]
    work: std::cell::Cell<usize>,
}

impl BlockIndex {
    pub(in crate::tui::ui) fn key_at(&self, at: usize) -> Option<RowKey> {
        self.visit(1);
        self.slots.get(&(self.head + at)).copied()
    }

    pub(in crate::tui::ui) fn is_local(&self, at: usize) -> bool {
        let at = self.head + at;
        self.visit(1);
        !self.slots.contains_key(&at) && !self.previews.contains_key(&at)
    }

    pub(in crate::tui::ui) fn local_anchor(&self, at: usize) -> Option<RowKey> {
        let at = self.head + at;
        self.visit(1);
        self.slots.range(..at).next_back().map(|(_, key)| *key)
    }

    pub(in crate::tui::ui) fn after(&self, key: Option<RowKey>, end: usize) -> usize {
        self.visit(1);
        key.map_or(0, |key| {
            self.keys
                .range((Bound::Excluded(key), Bound::Unbounded))
                .next()
                .map_or(end, |(_, at)| *at - self.head)
        })
    }

    pub(in crate::tui::ui) fn get(&self, key: RowKey) -> Option<usize> {
        self.visit(1);
        self.keys.get(&key).map(|at| *at - self.head)
    }

    pub(in crate::tui::ui) fn preview(&self, block: &ScrollBlock) -> Option<usize> {
        self.visit(1);
        identity(block)
            .and_then(|id| self.identities.get(&id).copied())
            .or_else(|| {
                self.previews
                    .iter()
                    .find(|(_, kind)| {
                        self.visit(1);
                        **kind == discriminant(block)
                    })
                    .map(|(at, _)| *at)
            })
            .map(|at| at - self.head)
    }

    pub(in crate::tui::ui) fn insertion(&self, key: RowKey, end: usize) -> usize {
        self.visit(1);
        self.keys
            .range(key..)
            .next()
            .map_or(end, |(_, at)| *at - self.head)
    }

    pub(in crate::tui::ui) fn group_end(&self, at: usize, end: usize) -> usize {
        let at = self.head + at;
        self.visit(2);
        let range = (Bound::Excluded(at), Bound::Unbounded);
        self.slots
            .range(range)
            .next()
            .map(|(at, _)| *at)
            .into_iter()
            .chain(self.previews.range(range).next().map(|(at, _)| *at))
            .min()
            .map_or(end, |at| at - self.head)
    }

    pub(in crate::tui::ui) fn bind(&mut self, key: RowKey, at: usize, block: &ScrollBlock) {
        let at = self.head + at;
        self.visit(1);
        if let Some(old) = self.slots.insert(at, key) {
            self.keys.remove(&old);
        }
        self.keys.insert(key, at);
        self.previews.remove(&at);
        self.remember_identity(at, block);
    }

    fn remember_identity(&mut self, at: usize, block: &ScrollBlock) {
        self.visit(1);
        if let Some(old) = self.identity_slots.remove(&at)
            && self.identities.get(&old) == Some(&at)
        {
            self.identities.remove(&old);
        }
        if let Some(id) = identity(block) {
            self.identity_slots.insert(at, id.clone());
            self.identities.insert(id, at);
        }
    }

    pub(in crate::tui::ui) fn note_preview(&mut self, at: usize, block: &ScrollBlock) {
        let at = self.head + at;
        self.visit(1);
        if !self.slots.contains_key(&at) {
            self.previews.entry(at).or_insert(discriminant(block));
        }
        self.remember_identity(at, block);
    }

    pub(in crate::tui::ui) fn pending(&self) -> Vec<usize> {
        self.visit(self.previews.len());
        self.previews.keys().map(|at| *at - self.head).collect()
    }

    pub(in crate::tui::ui) fn splice(&mut self, at: usize, removed: usize, added: usize) {
        let slot = self.head + at;
        self.visit(3);
        let prefix_untracked = self.slots.range(..slot).next().is_none()
            && self.identity_slots.range(..slot).next().is_none()
            && self.previews.range(..slot).next().is_none();
        if prefix_untracked && self.head + removed >= added {
            // Leading retention and its unkeyed truncation marker move the
            // coordinate origin, not every surviving historical slot.
            self.remap(at..at + removed, |_| None);
            self.head = self.head + removed - added;
            return;
        }
        let remap = |index: usize| {
            if index < at {
                Some(index)
            } else if index < at + removed {
                None
            } else {
                Some(index - removed + added)
            }
        };
        self.remap(at..usize::MAX, remap);
    }

    pub(in crate::tui::ui) fn remap(
        &mut self,
        range: Range<usize>,
        remap: impl Fn(usize) -> Option<usize>,
    ) {
        let head = self.head;
        let range = head + range.start..head.saturating_add(range.end);
        let keys = take_range(&mut self.slots, range.clone());
        self.visit(keys.len());
        for (at, key) in keys {
            self.keys.remove(&key);
            if let Some(next) = remap(at - head) {
                let next = head + next;
                self.keys.insert(key, next);
                self.slots.insert(next, key);
            }
        }
        let identities = take_range(&mut self.identity_slots, range.clone());
        self.visit(identities.len());
        for (at, id) in identities {
            if self.identities.get(&id) == Some(&at) {
                self.identities.remove(&id);
            }
            if let Some(next) = remap(at - head) {
                let next = head + next;
                self.identities.insert(id.clone(), next);
                self.identity_slots.insert(next, id);
            }
        }
        let previews = take_range(&mut self.previews, range);
        self.visit(previews.len());
        for (at, kind) in previews {
            if let Some(next) = remap(at - head) {
                let next = head + next;
                self.previews.insert(next, kind);
            }
        }
    }

    #[cfg(test)]
    pub(in crate::tui::ui) fn work(&self) -> usize {
        self.work.get()
    }

    fn visit(&self, count: usize) {
        #[cfg(test)]
        self.work.set(self.work.get() + count);
        #[cfg(not(test))]
        let _ = (self, count);
    }
}

fn take_range<T>(entries: &mut BTreeMap<usize, T>, range: Range<usize>) -> Vec<(usize, T)> {
    let indexes: Vec<_> = entries.range(range).map(|(at, _)| *at).collect();
    indexes
        .into_iter()
        .filter_map(|at| entries.remove(&at).map(|value| (at, value)))
        .collect()
}
