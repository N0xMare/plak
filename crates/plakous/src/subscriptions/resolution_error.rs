use super::{Error, ModelId, NativeSubscriptionError, fmt};

/// Credential-free failure from gated native model resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeModelResolutionError {
    /// Set when failure happened after a concrete model id was chosen.
    pub selected_model: Option<ModelId>,
    /// Underlying credential-free subscription failure (transport, model, etc.).
    pub kind: NativeSubscriptionError,
}

impl fmt::Display for NativeModelResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(f)
    }
}

impl Error for NativeModelResolutionError {}

impl NativeModelResolutionError {
    pub(super) fn before_selection(error: NativeSubscriptionError) -> Self {
        Self {
            selected_model: None,
            kind: error,
        }
    }

    pub(super) fn after_selection(selected_model: ModelId, error: NativeSubscriptionError) -> Self {
        let mut failure = Self::before_selection(error);
        failure.selected_model = Some(selected_model);
        failure
    }

    /// Returns the underlying subscription failure kind.
    #[must_use]
    pub fn kind(&self) -> NativeSubscriptionError {
        self.kind.clone()
    }
}
