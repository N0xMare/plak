//! Fixed, provider-neutral subscription layout identity.

use super::{NativeSubscriptionError, fmt};
use std::path::{Component, Path, PathBuf};

/// Fixed Plak-owned storage derived from one explicit XDG state root.
///
/// No environment, home directory, external login store, or provider CLI path
/// is consulted by this type.
#[derive(Clone)]
pub struct SubscriptionPaths {
    pub(super) state_root: PathBuf,
}

impl SubscriptionPaths {
    /// Binds the fixed Plak layout to one explicit absolute XDG state root.
    ///
    /// # Errors
    ///
    /// Rejects relative, empty, parent-traversing, or root-only paths.
    pub fn from_xdg_state_home(
        state_root: impl Into<PathBuf>,
    ) -> Result<Self, NativeSubscriptionError> {
        let state_root = state_root.into();
        validate_state_root(&state_root)?;
        Ok(Self { state_root })
    }
}

impl fmt::Debug for SubscriptionPaths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SubscriptionPaths(<redacted explicit state root>)")
    }
}
pub(super) fn validate_state_root(path: &Path) -> Result<(), NativeSubscriptionError> {
    if !path.is_absolute() || path.parent().is_none() || path == Path::new("/") {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    for component in path.components() {
        if matches!(component, Component::CurDir | Component::ParentDir) {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }
    }
    Ok(())
}
