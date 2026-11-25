//! Attachment queue accounting, including the initial retained replay.

use super::{Pending, ProtocolExtension};
use std::collections::VecDeque;

// Enough for both bounded transcript scopes and ordinary replies. A slow
// reader cannot multiply the bound by accumulating replacement snapshots.
pub(super) const MAX_QUEUE_BYTES: usize = 64 * 1024 * 1024;

pub(super) struct Queue<P: ProtocolExtension> {
    items: VecDeque<(Pending<P>, usize)>,
    bytes: usize,
    /// Kept observations count as bytes, but not as new live items.
    initial: usize,
}

impl<P: ProtocolExtension> Default for Queue<P> {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            bytes: 0,
            initial: 0,
        }
    }
}

impl<P: ProtocolExtension> Queue<P> {
    pub(super) const fn charge(encoded: usize) -> usize {
        encoded.saturating_add(size_of::<Pending<P>>())
    }

    pub(super) fn fits(&self, bytes: usize, capacity: usize, limit: usize) -> bool {
        self.live_items() < capacity && bytes <= limit.saturating_sub(self.bytes)
    }

    pub(super) fn can_append(
        &self,
        items: usize,
        bytes: usize,
        capacity: usize,
        limit: usize,
    ) -> bool {
        items <= capacity.saturating_sub(self.live_items())
            && bytes <= limit.saturating_sub(self.bytes)
    }

    pub(super) fn live_items(&self) -> usize {
        self.items.len() - self.initial
    }

    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &Pending<P>> {
        self.items.iter().map(|(item, _)| item)
    }

    pub(super) fn front(&self) -> Option<&Pending<P>> {
        self.items.front().map(|(item, _)| item)
    }

    pub(super) fn back(&self) -> Option<&Pending<P>> {
        self.items.back().map(|(item, _)| item)
    }

    pub(super) fn back_mut(&mut self) -> Option<&mut Pending<P>> {
        self.items.back_mut().map(|(item, _)| item)
    }

    pub(super) fn push(&mut self, item: Pending<P>, bytes: usize) {
        self.bytes += bytes;
        self.items.push_back((item, bytes));
    }

    pub(super) fn push_boundary(&mut self, item: Pending<P>) {
        self.push(item, 0);
    }

    pub(super) fn mark_initial(&mut self) {
        self.initial = self.items.len();
    }

    pub(super) fn remove(&mut self, index: usize) -> Option<Pending<P>> {
        let (item, cost) = self.items.remove(index)?;
        self.bytes -= cost;
        if index < self.initial {
            self.initial -= 1;
        }
        Some(item)
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&Pending<P>) -> bool) {
        let mut index = 0;
        let mut initial = 0;
        self.items.retain(|(item, cost)| {
            let keep = keep(item);
            initial += usize::from(keep && index < self.initial);
            index += 1;
            if !keep {
                self.bytes -= cost;
            }
            keep
        });
        self.initial = initial;
    }

    pub(super) fn take(&mut self, limit: usize) -> Vec<Pending<P>> {
        let count = limit.min(self.items.len());
        self.initial = self.initial.saturating_sub(count);
        self.items
            .drain(..count)
            .map(|(item, bytes)| {
                self.bytes -= bytes;
                item
            })
            .collect()
    }

    pub(super) fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
        self.initial = 0;
    }

    pub(super) const fn bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn len(&self) -> usize {
        self.items.len()
    }
}
