//! The parent's loss notice the driver hands out once per host generation
//! (spec 03 section 14.1). Process-local; nothing here is stored.

use super::{ProductAgentTree, TreeDriver};
use crate::tree::LossNoticeKey;
use std::collections::{BTreeMap, BTreeSet};

/// Which children this process named to their parent, and the notices it
/// offered whose command was not recorded yet.
#[derive(Debug, Default)]
pub(super) struct LossNotices {
    named: BTreeSet<LossNoticeKey>,
    /// One outstanding offer per parent: its exact text and its keys.
    offered: BTreeMap<String, (String, Vec<LossNoticeKey>)>,
}

impl TreeDriver {
    /// `parent`'s loss notice for its next command (spec 03 section 14.1):
    /// the children a lost host interrupted that this process has not named
    /// to it yet. Nothing is marked: the offer counts as delivered only once
    /// [`Self::confirm_loss_notice`] sees the command that carries it
    /// recorded, so a command that is never recorded loses nothing. Inside a
    /// mutation, pass that mutation's tree. A notice longer than `room` bytes
    /// is not offered.
    #[must_use]
    pub fn offer_loss_notice(
        &self,
        tree: Option<&ProductAgentTree>,
        parent: &str,
        room: usize,
    ) -> Option<String> {
        // Never held across the tree's mutex: a mutation passes its own tree
        // and takes only this lock.
        let notices = || {
            self.loss_noticed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let found = if let Some(tree) = tree {
            tree.loss_notice(parent, &notices().named)
        } else {
            let named = notices().named.clone();
            self.read(|tree| tree.loss_notice(parent, &named))
                .ok()
                .flatten()
        };
        let (text, keys) = found.filter(|(text, _)| text.len() <= room)?;
        notices()
            .offered
            .insert(parent.to_owned(), (text.clone(), keys));
        Some(text)
    }

    /// A command was recorded with exactly `command` as its text: each offer
    /// it ends with is delivered, and its children are named from now on.
    pub fn confirm_loss_notice(&self, command: &str) {
        let mut notices = self
            .loss_noticed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let delivered = notices
            .offered
            .iter()
            .filter(|(_, (text, _))| command.ends_with(text.as_str()))
            .map(|(parent, _)| parent.clone())
            .collect::<Vec<_>>();
        for parent in delivered {
            if let Some((_, keys)) = notices.offered.remove(&parent) {
                notices.named.extend(keys);
            }
        }
    }
}
