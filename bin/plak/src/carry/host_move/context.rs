//! Committed selection and display facts, without old canonical hashes or grants.
use plakous::model::{Digest, DigestDomain};
use plakous::sessions::host_move::{ImportedCompactionFacts, MoveCompactionStrategy};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Compaction {
    source_checkpoint: String,
    source_operation: Option<Digest>,
    summary_entry: String,
    prefix_segments: u32,
    trailing_segments: u32,
    strategy: Option<Strategy>,
    replaced_segments: Option<u32>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Strategy {
    SemanticSummary,
    Native,
    Placeholder,
    ReplayOmission,
    Residency,
}

impl From<MoveCompactionStrategy> for Strategy {
    fn from(value: MoveCompactionStrategy) -> Self {
        match value {
            MoveCompactionStrategy::SemanticSummaryPrefixV1 => Self::SemanticSummary,
            MoveCompactionStrategy::NativeResponsesTriggerV1 => Self::Native,
            MoveCompactionStrategy::ToolResultPlaceholdersV1 => Self::Placeholder,
            MoveCompactionStrategy::ReplayOmissionV1 => Self::ReplayOmission,
            MoveCompactionStrategy::ResidencyPlaceholdersV1 => Self::Residency,
        }
    }
}

impl From<Strategy> for MoveCompactionStrategy {
    fn from(value: Strategy) -> Self {
        match value {
            Strategy::SemanticSummary => Self::SemanticSummaryPrefixV1,
            Strategy::Native => Self::NativeResponsesTriggerV1,
            Strategy::Placeholder => Self::ToolResultPlaceholdersV1,
            Strategy::ReplayOmission => Self::ReplayOmissionV1,
            Strategy::Residency => Self::ResidencyPlaceholdersV1,
        }
    }
}

impl Compaction {
    #[cfg(test)]
    pub(crate) fn capture(facts: ImportedCompactionFacts) -> Self {
        Self {
            source_checkpoint: facts.source_checkpoint.to_string(),
            source_operation: facts.source_operation,
            summary_entry: facts.summary_entry.to_string(),
            prefix_segments: facts.prefix_segments,
            trailing_segments: facts.trailing_segments,
            strategy: facts.strategy.map(Into::into),
            replaced_segments: facts.replaced_segments,
        }
    }

    pub(crate) fn decode(self) -> Result<ImportedCompactionFacts, String> {
        if self.prefix_segments == 0
            || self.prefix_segments > 100_000
            || self.trailing_segments >= self.prefix_segments
            || self
                .source_operation
                .is_some_and(|digest| digest.domain() != DigestDomain::ProviderOperation)
            || self.strategy.is_some() != self.replaced_segments.is_some()
            || self
                .replaced_segments
                .is_some_and(|count| count == 0 || count > 100_000)
        {
            return Err("invalid move compaction selection".into());
        }
        Ok(ImportedCompactionFacts {
            source_checkpoint: self
                .source_checkpoint
                .parse()
                .map_err(|_| "invalid move compaction source")?,
            source_operation: self.source_operation,
            summary_entry: self
                .summary_entry
                .parse()
                .map_err(|_| "invalid move compaction summary")?,
            prefix_segments: self.prefix_segments,
            trailing_segments: self.trailing_segments,
            strategy: self.strategy.map(Into::into),
            replaced_segments: self.replaced_segments,
        })
    }
}
