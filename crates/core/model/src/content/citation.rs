use serde::{Deserialize, Deserializer, Serialize};

use crate::EntryId;

use super::{
    ContentError, MAX_CORRELATION_ID_BYTES, MAX_MEDIA_URI_BYTES, ensure_maximum, ensure_nonempty,
};

/// A unit used to interpret citation offsets.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum CitationUnit {
    /// Unicode scalar-value offsets.
    UnicodeScalar,
    /// UTF-8 byte offsets.
    Utf8Byte,
    /// UTF-16 code-unit offsets.
    Utf16CodeUnit,
}

/// An explicit citation span.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CitationRange {
    start: u64,
    end: u64,
    unit: CitationUnit,
}

#[derive(Deserialize)]
struct CitationRangeWire {
    start: u64,
    end: u64,
    unit: CitationUnit,
}

impl<'de> Deserialize<'de> for CitationRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CitationRangeWire::deserialize(deserializer)?;
        Self::new(wire.start, wire.end, wire.unit).map_err(serde::de::Error::custom)
    }
}

impl CitationRange {
    /// Creates a half-open citation range.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::InvalidCitationRange`] when `end` precedes `start`.
    pub fn new(start: u64, end: u64, unit: CitationUnit) -> Result<Self, ContentError> {
        if end < start {
            return Err(ContentError::InvalidCitationRange);
        }
        Ok(Self { start, end, unit })
    }

    /// Returns the inclusive start offset.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns the exclusive end offset.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Returns the offset unit.
    #[must_use]
    pub const fn unit(self) -> CitationUnit {
        self.unit
    }
}

/// A stable target inside a canonical conversation item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CitationTarget {
    item: EntryId,
    block_index: u32,
}

impl CitationTarget {
    /// Creates a target for one block in an item.
    #[must_use]
    pub const fn new(item: EntryId, block_index: u32) -> Self {
        Self { item, block_index }
    }

    /// Returns the target item.
    #[must_use]
    pub const fn item(self) -> EntryId {
        self.item
    }

    /// Returns the target block index.
    #[must_use]
    pub const fn block_index(self) -> u32 {
        self.block_index
    }
}

/// Citation source identity and presentation metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CitationSource {
    source_id: String,
    annotation_id: String,
    uri: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
struct CitationSourceWire {
    source_id: String,
    annotation_id: String,
    uri: Option<String>,
    title: Option<String>,
}

impl<'de> Deserialize<'de> for CitationSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CitationSourceWire::deserialize(deserializer)?;
        Self::new(wire.source_id, wire.annotation_id, wire.uri, wire.title)
            .map_err(serde::de::Error::custom)
    }
}

impl CitationSource {
    /// Creates citation source metadata with stable non-empty identities.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when a required identity or present
    /// presentation value is empty.
    pub fn new(
        source_id: impl Into<String>,
        annotation_id: impl Into<String>,
        uri: Option<String>,
        title: Option<String>,
    ) -> Result<Self, ContentError> {
        let source_id = source_id.into();
        let annotation_id = annotation_id.into();
        ensure_nonempty(&source_id, "citation source ID")?;
        ensure_nonempty(&annotation_id, "citation annotation ID")?;
        ensure_maximum(
            source_id.len(),
            "citation source ID",
            MAX_CORRELATION_ID_BYTES,
        )?;
        ensure_maximum(
            annotation_id.len(),
            "citation annotation ID",
            MAX_CORRELATION_ID_BYTES,
        )?;
        if uri.as_deref().is_some_and(str::is_empty) {
            return Err(ContentError::Empty {
                field: "citation URI",
            });
        }
        if title.as_deref().is_some_and(str::is_empty) {
            return Err(ContentError::Empty {
                field: "citation title",
            });
        }
        if let Some(uri) = &uri {
            ensure_maximum(uri.len(), "citation URI", MAX_MEDIA_URI_BYTES)?;
        }
        if let Some(title) = &title {
            ensure_maximum(title.len(), "citation title", MAX_CORRELATION_ID_BYTES)?;
        }
        Ok(Self {
            source_id,
            annotation_id,
            uri,
            title,
        })
    }

    /// Returns stable source identity.
    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Returns stable annotation identity.
    #[must_use]
    pub fn annotation_id(&self) -> &str {
        &self.annotation_id
    }

    /// Returns the optional source URI.
    #[must_use]
    pub fn uri(&self) -> Option<&str> {
        self.uri.as_deref()
    }

    /// Returns the optional display title.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

/// A citation retaining target association and explicit offset units.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CitationBlock {
    target: CitationTarget,
    source: CitationSource,
    range: Option<CitationRange>,
}

impl CitationBlock {
    /// Creates a citation.
    #[must_use]
    pub const fn new(
        target: CitationTarget,
        source: CitationSource,
        range: Option<CitationRange>,
    ) -> Self {
        Self {
            target,
            source,
            range,
        }
    }

    /// Returns the cited target.
    #[must_use]
    pub const fn target(&self) -> CitationTarget {
        self.target
    }

    /// Returns source metadata.
    #[must_use]
    pub const fn source(&self) -> &CitationSource {
        &self.source
    }

    /// Returns the optional target range.
    #[must_use]
    pub const fn range(&self) -> Option<CitationRange> {
        self.range
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.source.source_id.len()
            + self.source.annotation_id.len()
            + self.source.uri.as_deref().map_or(0, str::len)
            + self.source.title.as_deref().map_or(0, str::len)
    }
}
