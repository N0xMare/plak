//! Keyed, revisioned transcript updates shared by every presentation scope.

use std::collections::{BTreeMap, BTreeSet};

use super::{RowKey, TranscriptRow};
use crate::views::presentation::{
    MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, PresentationError, PresentationSnapshot, row_bytes,
};

/// The transcript an update belongs to; child content never mutates the root.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum TranscriptScope {
    /// The attached product session.
    #[default]
    Root,
    /// One agent of the attached session's tree.
    Agent(String),
}

/// A row and its immutable ordering identity within one transcript scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptItem {
    /// Ordering identity, not a display label or vector index.
    pub key: RowKey,
    /// The one transcript row vocabulary.
    pub row: TranscriptRow,
}

/// An atomic update to one exact presentation revision. Missing a revision
/// requires a scope snapshot; a patch is never recovery or terminal authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptPatch {
    /// The transcript to update.
    pub scope: TranscriptScope,
    /// The exact baseline this patch was produced against.
    pub from_revision: u64,
    /// The new revision, strictly greater than the baseline.
    pub to_revision: u64,
    /// New or changed rows, in strictly increasing key order.
    pub upserts: Vec<TranscriptItem>,
    /// Removed keys, in strictly increasing order and disjoint from upserts.
    pub deletes: Vec<RowKey>,
}

impl TranscriptPatch {
    /// Computes the keyed difference between two bounded presentations.
    #[must_use]
    pub fn between(
        scope: TranscriptScope,
        from_revision: u64,
        to_revision: u64,
        before: &BTreeMap<RowKey, TranscriptRow>,
        after: &BTreeMap<RowKey, TranscriptRow>,
    ) -> Self {
        Self {
            scope,
            from_revision,
            to_revision,
            upserts: after
                .iter()
                .filter(|(key, row)| before.get(key) != Some(row))
                .map(|(key, row)| TranscriptItem {
                    key: *key,
                    row: row.clone(),
                })
                .collect(),
            deletes: before
                .keys()
                .filter(|key| !after.contains_key(key))
                .copied()
                .collect(),
        }
    }

    /// Checks patch shape without interpreting a baseline.
    ///
    /// # Errors
    /// Returns a non-advancing revision, unordered/duplicate/conflicting keys,
    /// or a patch exceeding the presentation bounds.
    pub fn validate(&self) -> Result<(), PresentationError> {
        if self.to_revision <= self.from_revision {
            return Err(PresentationError::NonAdvancingRevision);
        }
        validate_items(&self.upserts)?;
        if self.deletes.len() > MAX_SNAPSHOT_ROWS {
            return Err(PresentationError::ItemLimit);
        }
        if self.deletes.windows(2).any(|keys| keys[0] >= keys[1]) {
            return Err(PresentationError::KeyOrder);
        }
        let deletes: BTreeSet<_> = self.deletes.iter().collect();
        if self.upserts.iter().any(|item| deletes.contains(&item.key)) {
            return Err(PresentationError::ConflictingKey);
        }
        Ok(())
    }
}

pub(in crate::views) fn validate_items(items: &[TranscriptItem]) -> Result<(), PresentationError> {
    if items.len() > MAX_SNAPSHOT_ROWS {
        return Err(PresentationError::ItemLimit);
    }
    if items.windows(2).any(|items| items[0].key >= items[1].key) {
        return Err(PresentationError::KeyOrder);
    }
    if items
        .iter()
        .map(|item| row_bytes(&item.row))
        .fold(0, usize::saturating_add)
        > MAX_PRESENTATION_BYTES
    {
        return Err(PresentationError::ByteLimit);
    }
    Ok(())
}

/// A bounded keyed transcript baseline. Applying a patch touches only its
/// keys and validates the resulting bounds before mutating anything.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TranscriptState {
    scope: TranscriptScope,
    revision: u64,
    items: BTreeMap<RowKey, TranscriptRow>,
    bytes: usize,
}

impl TranscriptState {
    /// Installs a complete baseline.
    ///
    /// # Errors
    /// Returns the snapshot's schema, ordering or bound violation.
    pub fn replace(&mut self, snapshot: PresentationSnapshot) -> Result<(), PresentationError> {
        snapshot.validate()?;
        self.bytes = snapshot.items.iter().map(|item| row_bytes(&item.row)).sum();
        self.items = snapshot
            .items
            .into_iter()
            .map(|item| (item.key, item.row))
            .collect();
        self.scope = snapshot.scope;
        self.revision = snapshot.revision;
        Ok(())
    }

    /// The current scope.
    #[must_use]
    pub const fn scope(&self) -> &TranscriptScope {
        &self.scope
    }

    /// The exact revision the next patch must name.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Rows in immutable key order; UI state belongs beside these values,
    /// never inside a second copy of their content.
    #[must_use]
    pub const fn items(&self) -> &BTreeMap<RowKey, TranscriptRow> {
        &self.items
    }

    /// Applies one exact patch atomically. Refusal leaves the baseline intact.
    ///
    /// # Errors
    /// Returns a scope/revision mismatch, malformed patch or resulting bound
    /// violation. The consumer must resync rather than fold a later patch.
    pub fn apply(&mut self, patch: TranscriptPatch) -> Result<(), PresentationError> {
        if patch.scope != self.scope {
            return Err(PresentationError::ScopeMismatch);
        }
        if patch.from_revision != self.revision {
            return Err(PresentationError::RevisionMismatch {
                expected: self.revision,
                actual: patch.from_revision,
            });
        }
        patch.validate()?;
        let mut count = self.items.len();
        let mut bytes = self.bytes;
        for key in &patch.deletes {
            if let Some(row) = self.items.get(key) {
                count -= 1;
                bytes -= row_bytes(row);
            }
        }
        for item in &patch.upserts {
            if let Some(row) = self.items.get(&item.key) {
                bytes -= row_bytes(row);
            } else {
                count += 1;
            }
            bytes = bytes.saturating_add(row_bytes(&item.row));
        }
        if count > MAX_SNAPSHOT_ROWS {
            return Err(PresentationError::ItemLimit);
        }
        if bytes > MAX_PRESENTATION_BYTES {
            return Err(PresentationError::ByteLimit);
        }
        for key in patch.deletes {
            self.items.remove(&key);
        }
        for item in patch.upserts {
            self.items.insert(item.key, item.row);
        }
        self.bytes = bytes;
        self.revision = patch.to_revision;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
