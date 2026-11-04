//! Stable transcript ordering shared by projection and presentation.

use serde::{Deserialize, Serialize};

/// Which writer a row key belongs to.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// A row the projector derived from a feed record.
    Runtime,
    /// An operator notice or an imported row.
    Product,
}

impl Lane {
    /// The stored lane number.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::Runtime => 0,
            Self::Product => 1,
        }
    }

    /// Parses a stored lane number.
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Runtime),
            1 => Some(Self::Product),
            _ => None,
        }
    }
}

/// A transcript row's immutable ordering key: (anchor sequence, lane,
/// ordinal). The transcript is ordered by it.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    schemars::JsonSchema,
)]
pub struct RowKey {
    /// The record sequence (lane 0) or the applied cursor (lane 1).
    pub anchor: u64,
    /// The writer.
    pub lane: Lane,
    /// The position among the rows of that anchor and lane.
    pub ordinal: u32,
}

impl RowKey {
    /// The key of the truncation marker: before every other row.
    pub const TRUNCATION: Self = Self::new(0, Lane::Runtime, 0);

    /// A key.
    #[must_use]
    pub const fn new(anchor: u64, lane: Lane, ordinal: u32) -> Self {
        Self {
            anchor,
            lane,
            ordinal,
        }
    }
}
