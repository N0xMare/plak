//! Neutral aggregate image bounds; these do not alter token metering.

/// Invalid aggregate resident-image limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("resident image budgets must be nonzero")]
pub struct ResidentImageBudgetError;

/// Maximum resident image count and known encoded payload bytes in one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResidentImageBudget {
    max_images: u32,
    max_encoded_bytes: u64,
}

impl Default for ResidentImageBudget {
    fn default() -> Self {
        Self {
            max_images: 20,
            max_encoded_bytes: 16 * 1024 * 1024,
        }
    }
}

impl ResidentImageBudget {
    /// Constructs nonzero aggregate bounds. Unknown remote bytes are not zero
    /// measurements; external images still count against the image limit.
    ///
    /// # Errors
    /// Rejects a zero count or encoded-byte limit.
    pub fn new(max_images: u32, max_encoded_bytes: u64) -> Result<Self, ResidentImageBudgetError> {
        if max_images == 0 || max_encoded_bytes == 0 {
            return Err(ResidentImageBudgetError);
        }
        Ok(Self {
            max_images,
            max_encoded_bytes,
        })
    }

    /// Maximum image count, including protected attachments.
    #[must_use]
    pub const fn max_images(self) -> u32 {
        self.max_images
    }

    /// Maximum known encoded payload bytes, excluding framing and text.
    #[must_use]
    pub const fn max_encoded_bytes(self) -> u64 {
        self.max_encoded_bytes
    }
}
