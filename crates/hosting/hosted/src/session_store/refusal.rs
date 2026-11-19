use crate::error::StoreRefusal;

/// A product-store failure. With a [`StoreRefusal`] it wrote nothing and the
/// same call may succeed when retried; without one it is final for the call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordStoreError {
    message: String,
    refusal: Option<StoreRefusal>,
}

impl RecordStoreError {
    /// A transient refusal: nothing was written.
    pub fn refused(refusal: StoreRefusal, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            refusal: Some(refusal),
        }
    }

    /// The transient refusal class, when the failure is one.
    #[must_use]
    pub const fn refusal(&self) -> Option<StoreRefusal> {
        self.refusal
    }

    /// Human-readable failure message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for RecordStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RecordStoreError {}

impl From<String> for RecordStoreError {
    fn from(message: String) -> Self {
        Self {
            message,
            refusal: None,
        }
    }
}

impl From<&str> for RecordStoreError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl From<RecordStoreError> for String {
    fn from(error: RecordStoreError) -> Self {
        error.message
    }
}
