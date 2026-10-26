//! Revision and bounds bookkeeping without a second owner of row content.

use std::collections::BTreeMap;

use super::{RowKey, TranscriptPatch, TranscriptRow, TranscriptScope};
use crate::views::presentation::{
    MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, PresentationError, PresentationSnapshot, row_bytes,
};

/// A scoped transcript revision with key sizes and canonical tail
/// classification, without owning row payloads. Consumers may move those
/// payloads into their retained presentation without cloning them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TranscriptRevision {
    scope: TranscriptScope,
    revision: u64,
    entries: BTreeMap<RowKey, RowMetadata>,
    bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RowMetadata {
    bytes: usize,
    assistant: bool,
}

impl RowMetadata {
    fn new(row: &TranscriptRow) -> Self {
        Self {
            bytes: row_bytes(row),
            assistant: matches!(row, TranscriptRow::Assistant { .. }),
        }
    }
}

impl TranscriptRevision {
    /// Installs a complete revision without retaining its row payloads.
    ///
    /// # Errors
    /// Returns the snapshot's schema, ordering or bound violation. Refusal
    /// leaves the previous revision intact.
    pub fn replace(&mut self, snapshot: &PresentationSnapshot) -> Result<(), PresentationError> {
        snapshot.validate()?;
        self.entries = snapshot
            .items
            .iter()
            .map(|item| (item.key, RowMetadata::new(&item.row)))
            .collect();
        self.bytes = self.entries.values().map(|entry| entry.bytes).sum();
        self.scope.clone_from(&snapshot.scope);
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

    /// Whether the newest canonical row is assistant prose, including when
    /// a consumer's live view has since appended or discarded its text.
    #[must_use]
    pub fn last_row_is_assistant(&self) -> Option<bool> {
        self.entries
            .last_key_value()
            .map(|(_, entry)| entry.assistant)
    }

    /// Advances one exact patch, validating the resulting key/byte bounds
    /// before mutating anything. Row ownership stays with the caller.
    ///
    /// # Errors
    /// Returns a scope/revision mismatch, malformed patch or resulting bound
    /// violation. Refusal leaves the previous revision intact.
    pub fn apply(&mut self, patch: &TranscriptPatch) -> Result<(), PresentationError> {
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
        // Measuring a row serializes its payload. Reuse that measurement
        // when committing rather than serializing each upsert twice here.
        let upserts: Vec<_> = patch
            .upserts
            .iter()
            .map(|item| (item.key, RowMetadata::new(&item.row)))
            .collect();
        let mut count = self.entries.len();
        let mut bytes = self.bytes;
        for key in &patch.deletes {
            if let Some(entry) = self.entries.get(key) {
                count -= 1;
                bytes -= entry.bytes;
            }
        }
        for (key, added) in &upserts {
            if let Some(entry) = self.entries.get(key) {
                bytes -= entry.bytes;
            } else {
                count += 1;
            }
            bytes = bytes.saturating_add(added.bytes);
        }
        if count > MAX_SNAPSHOT_ROWS {
            return Err(PresentationError::ItemLimit);
        }
        if bytes > MAX_PRESENTATION_BYTES {
            return Err(PresentationError::ByteLimit);
        }
        for key in &patch.deletes {
            self.entries.remove(key);
        }
        for (key, entry) in upserts {
            self.entries.insert(key, entry);
        }
        self.bytes = bytes;
        self.revision = patch.to_revision;
        Ok(())
    }
}
