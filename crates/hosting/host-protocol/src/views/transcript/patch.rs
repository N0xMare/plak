//! Keyed, revisioned transcript updates shared by every presentation scope.

use std::collections::{BTreeMap, BTreeSet};

use super::{RowKey, ToolItem, TranscriptRevision, TranscriptRow};

/// Uncommitted appends between keyed publication boundaries. These are not
/// revisions: a snapshot contains their accumulated resync baseline.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptStreamDelta {
    /// Assistant prose appended to the current output.
    Assistant(String),
    /// The current assistant output ended; the next append starts a new row.
    AssistantDone,
    /// Admitted reasoning appended at one output location.
    Reasoning {
        /// Opaque output location.
        location_key: String,
        /// Already-redacted text.
        text: String,
    },
    /// The latest live tool presentation.
    Tool(Box<ToolItem>),
}
use crate::views::presentation::{
    MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, PresentationError, PresentationSnapshot, row_bytes,
};

/// The transcript an update belongs to; child content never mutates the root.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd,
)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptScope {
    /// The attached product session.
    #[default]
    Root,
    /// One agent of the attached session's tree.
    Agent(String),
}

/// A row and its immutable ordering identity within one transcript scope.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TranscriptItem {
    /// Ordering identity, not a display label or vector index.
    pub key: RowKey,
    /// The one transcript row vocabulary.
    pub row: TranscriptRow,
}

/// An atomic update to one exact presentation revision. Missing a revision
/// requires a scope snapshot; a patch is never recovery or terminal authority.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(try_from = "TranscriptPatchFields")]
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

/// A patch as it arrives, before [`TranscriptPatch::validate`].
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptPatchFields {
    scope: TranscriptScope,
    from_revision: u64,
    to_revision: u64,
    upserts: Vec<TranscriptItem>,
    deletes: Vec<RowKey>,
}

/// A peer cannot hand over a patch the host would refuse to emit.
impl TryFrom<TranscriptPatchFields> for TranscriptPatch {
    type Error = String;

    fn try_from(fields: TranscriptPatchFields) -> Result<Self, String> {
        let patch = Self {
            scope: fields.scope,
            from_revision: fields.from_revision,
            to_revision: fields.to_revision,
            upserts: fields.upserts,
            deletes: fields.deletes,
        };
        patch
            .validate()
            .map_err(|error| format!("invalid transcript patch: {error:?}"))?;
        Ok(patch)
    }
}

impl TranscriptPatch {
    /// Composes consecutive updates without retaining overwritten row copies.
    /// Refuses a gap, different scope, malformed input or an oversized result.
    #[must_use]
    pub fn merged(&self, next: &Self) -> Option<Self> {
        if self.scope != next.scope || self.to_revision != next.from_revision {
            return None;
        }
        self.validate().ok()?;
        next.validate().ok()?;
        let mut upserts: BTreeMap<_, _> = self
            .upserts
            .iter()
            .map(|item| (item.key, item.row.clone()))
            .collect();
        let mut deletes: BTreeSet<_> = self.deletes.iter().copied().collect();
        for key in &next.deletes {
            upserts.remove(key);
            deletes.insert(*key);
        }
        for item in &next.upserts {
            deletes.remove(&item.key);
            upserts.insert(item.key, item.row.clone());
        }
        let merged = Self {
            scope: self.scope.clone(),
            from_revision: self.from_revision,
            to_revision: next.to_revision,
            upserts: upserts
                .into_iter()
                .map(|(key, row)| TranscriptItem { key, row })
                .collect(),
            deletes: deletes.into_iter().collect(),
        };
        merged.validate().ok()?;
        Some(merged)
    }

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
    version: TranscriptRevision,
    items: BTreeMap<RowKey, TranscriptRow>,
}

impl TranscriptState {
    /// Installs a complete baseline.
    ///
    /// # Errors
    /// Returns the snapshot's schema, ordering or bound violation.
    pub fn replace(&mut self, snapshot: PresentationSnapshot) -> Result<(), PresentationError> {
        self.version.replace(&snapshot)?;
        self.items = snapshot
            .items
            .into_iter()
            .map(|item| (item.key, item.row))
            .collect();
        Ok(())
    }

    /// The current scope.
    #[must_use]
    pub const fn scope(&self) -> &TranscriptScope {
        self.version.scope()
    }

    /// The exact revision the next patch must name.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.version.revision()
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
        self.version.apply(&patch)?;
        for key in patch.deletes {
            self.items.remove(&key);
        }
        for item in patch.upserts {
            self.items.insert(item.key, item.row);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
