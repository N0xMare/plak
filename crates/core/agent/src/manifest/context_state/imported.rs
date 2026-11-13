//! Semantic committed selections, never reconstructed historical admission evidence.
use super::{
    CanonicalHistory, CheckpointId, Digest, DigestDomain, EntryId, HistorySegment,
    HistorySegmentKind, MAX_RUNTIME_RECORDS, Reader, RuntimeCompactionStrategy,
    RuntimeContextState, RuntimeManifestError, Writer, runtime_context_state_identity,
    validate_digest,
};

/// Unsupported host-move selection, expressed without native canonical hashes.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedCompactionFacts {
    pub source_checkpoint: CheckpointId,
    pub source_operation: Option<Digest>,
    pub summary_entry: EntryId,
    pub prefix_segments: u32,
    pub trailing_segments: u32,
    pub strategy: Option<RuntimeCompactionStrategy>,
    pub replaced_segments: Option<u32>,
}

/// Current-native identities derived from semantic imported facts.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedCompactionSelection {
    facts: ImportedCompactionFacts,
    prefix: Digest,
    summary: Digest,
}

impl ImportedCompactionFacts {
    fn validate(&self) -> Result<(), RuntimeManifestError> {
        if let Some(operation) = self.source_operation {
            validate_digest(operation, DigestDomain::ProviderOperation)?;
        }
        let bound =
            u32::try_from(MAX_RUNTIME_RECORDS).map_err(|_| RuntimeManifestError::RecordLimit)?;
        if self.prefix_segments == 0
            || self.prefix_segments > bound
            || self.trailing_segments >= self.prefix_segments
            || self.strategy.is_some() != self.replaced_segments.is_some()
            || self
                .replaced_segments
                .is_some_and(|count| count == 0 || count > bound)
        {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        Ok(())
    }

    fn summary<'a>(
        &self,
        history: &'a CanonicalHistory,
    ) -> Result<&'a HistorySegment, RuntimeManifestError> {
        self.validate()?;
        let prefix_len = self.prefix_segments as usize;
        if history.segments().len() < prefix_len {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let mut matches = history
            .segments()
            .iter()
            .take(prefix_len)
            .enumerate()
            .filter(|(_, segment)| {
                segment.kind() == HistorySegmentKind::Compaction
                    && segment
                        .items()
                        .iter()
                        .any(|item| item.id() == self.summary_entry)
            });
        let (index, summary) = matches
            .next()
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?;
        if matches.next().is_some() || self.trailing_segments as usize > prefix_len - index - 1 {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        Ok(summary)
    }
}

impl ImportedCompactionSelection {
    pub(super) fn new(
        facts: ImportedCompactionFacts,
        history: &CanonicalHistory,
    ) -> Result<Self, RuntimeManifestError> {
        let summary = facts.summary(history)?.identity();
        let prefix = history
            .segment_prefix_identity(facts.prefix_segments as usize)
            .map_err(|_| RuntimeManifestError::InvalidCompactionProvenance)?;
        Ok(Self {
            facts,
            prefix,
            summary,
        })
    }

    #[must_use]
    pub const fn facts(&self) -> &ImportedCompactionFacts {
        &self.facts
    }

    /// Current-native summary identity, recomputed from the imported semantic history.
    #[must_use]
    pub const fn summary_identity(&self) -> Digest {
        self.summary
    }

    pub(super) fn validate(&self) -> Result<(), RuntimeManifestError> {
        self.facts.validate()?;
        validate_digest(self.prefix, DigestDomain::CanonicalHistory)?;
        validate_digest(self.summary, DigestDomain::CanonicalHistorySegment)
    }

    pub(super) fn validate_history(
        &self,
        history: &CanonicalHistory,
    ) -> Result<(), RuntimeManifestError> {
        if Self::new(self.facts.clone(), history)? != *self {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        Ok(())
    }

    pub(super) fn write(&self, writer: &mut Writer) -> Result<(), RuntimeManifestError> {
        self.validate()?;
        writer.id(self.facts.source_checkpoint.as_uuid());
        writer.optional(self.facts.source_operation.is_some());
        if let Some(operation) = self.facts.source_operation {
            writer.digest(operation)?;
        }
        writer.id(self.facts.summary_entry.as_uuid());
        writer.u32(self.facts.prefix_segments);
        writer.u32(self.facts.trailing_segments);
        writer.optional(self.facts.strategy.is_some());
        if let Some(strategy) = self.facts.strategy {
            writer.u8(strategy.tag());
            writer.u32(
                self.facts
                    .replaced_segments
                    .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?,
            );
        }
        writer.digest(self.prefix)?;
        writer.digest(self.summary)
    }

    pub(super) fn read(reader: &mut Reader<'_>) -> Result<Self, RuntimeManifestError> {
        let source_checkpoint = reader.checkpoint_id()?;
        let source_operation = reader
            .optional()?
            .then(|| reader.digest(Some(DigestDomain::ProviderOperation)))
            .transpose()?;
        let summary_entry = reader.entry_id()?;
        let prefix_segments = reader.u32()?;
        let trailing_segments = reader.u32()?;
        let recorded = reader
            .optional()?
            .then(|| {
                Ok::<_, RuntimeManifestError>((
                    RuntimeCompactionStrategy::from_tag(reader.u8()?)?,
                    reader.u32()?,
                ))
            })
            .transpose()?;
        let selection = Self {
            facts: ImportedCompactionFacts {
                source_checkpoint,
                source_operation,
                summary_entry,
                prefix_segments,
                trailing_segments,
                strategy: recorded.map(|(strategy, _)| strategy),
                replaced_segments: recorded.map(|(_, count)| count),
            },
            prefix: reader.digest(Some(DigestDomain::CanonicalHistory))?,
            summary: reader.digest(Some(DigestDomain::CanonicalHistorySegment))?,
        };
        selection.validate()?;
        Ok(selection)
    }
}

impl RuntimeContextState {
    /// Projects only committed semantic selection; absent audit facts stay absent.
    #[doc(hidden)]
    pub fn export_compaction_selection(
        &self,
        source_checkpoint: CheckpointId,
        history: &CanonicalHistory,
    ) -> Result<Option<ImportedCompactionFacts>, RuntimeManifestError> {
        self.validate_history(history)?;
        if let Some(imported) = &self.imported_compaction {
            return Ok(Some(imported.facts.clone()));
        }
        let mut summaries = history
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Compaction);
        let Some(summary) = summaries.next() else {
            return Ok(None);
        };
        if summaries.next().is_some() {
            return Err(RuntimeManifestError::InvalidCompactionProvenance);
        }
        let entry = summary
            .items()
            .first()
            .ok_or(RuntimeManifestError::InvalidCompactionProvenance)?
            .id();
        let (checkpoint, operation, prefix_segments, trailing_segments) =
            self.compaction.as_ref().map_or(
                (
                    source_checkpoint,
                    None,
                    u32::try_from(history.segments().len())
                        .map_err(|_| RuntimeManifestError::RecordLimit)?,
                    0,
                ),
                |proof| {
                    (
                        proof.source_checkpoint,
                        Some(proof.operation),
                        proof.target_segment_count,
                        proof.trailing_segment_count,
                    )
                },
            );
        let facts = ImportedCompactionFacts {
            source_checkpoint: checkpoint,
            source_operation: operation,
            summary_entry: entry,
            prefix_segments,
            trailing_segments,
            strategy: self.compaction.as_ref().map(|proof| proof.strategy),
            replaced_segments: self
                .compaction
                .as_ref()
                .map(|proof| proof.selected_segment_count),
        };
        facts.summary(history)?;
        Ok(Some(facts))
    }

    /// Reconstructs only committed selection facts; never source cache/admission grants.
    #[doc(hidden)]
    pub(crate) fn with_imported_compaction(
        &self,
        facts: ImportedCompactionFacts,
        history: &CanonicalHistory,
    ) -> Result<Self, RuntimeManifestError> {
        if self.compaction.is_some()
            || self.imported_compaction.is_some()
            || self.transition.is_some()
        {
            return Err(RuntimeManifestError::InvalidContextState);
        }
        let mut state = self.clone();
        state.occupancy_anchor = None;
        state.request_anchor = None;
        state.imported_compaction =
            Some(Box::new(ImportedCompactionSelection::new(facts, history)?));
        state.identity = runtime_context_state_identity(&state)?;
        Ok(state)
    }

    #[doc(hidden)]
    #[must_use]
    pub fn imported_compaction(&self) -> Option<&ImportedCompactionSelection> {
        self.imported_compaction.as_deref()
    }
}

#[cfg(test)]
mod tests;
