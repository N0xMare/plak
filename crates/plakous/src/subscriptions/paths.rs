//! Fixed, provider-neutral subscription layout identity.

use super::{NativeProvider, NativeSubscriptionError, fmt};
use std::path::{Component, Path, PathBuf};

/// Fixed Plak-owned storage derived from one explicit XDG state root.
///
/// No environment, home directory, external login store, or provider CLI path
/// is consulted by this type.
#[derive(Clone)]
pub struct SubscriptionPaths {
    pub(super) state_root: PathBuf,
    pub(super) named: Option<NamedSlot>,
}

#[derive(Clone)]
pub(super) struct NamedSlot {
    pub(super) provider: NativeProvider,
    pub(super) vertical: &'static str,
    pub(super) name: String,
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
        Ok(Self {
            state_root,
            named: None,
        })
    }

    /// Selects one validated local name within an exact subscription vertical.
    /// This is offline and does not create or inspect the slot.
    ///
    /// # Errors
    /// Rejects an invalid or reserved name and an unknown vertical.
    pub fn for_named_account(
        &self,
        provider: NativeProvider,
        name: &str,
    ) -> Result<Self, NativeSubscriptionError> {
        if name.is_empty()
            || name.len() > 32
            || name == "default"
            || !name.as_bytes()[0].is_ascii_lowercase()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(NativeSubscriptionError::AuthenticationBinding);
        }
        let vertical = match provider {
            NativeProvider::Codex => "codex",
            NativeProvider::Xai => "grok",
            NativeProvider::Claude => "claude",
        };
        Ok(Self {
            state_root: self.state_root.clone(),
            named: Some(NamedSlot {
                provider,
                vertical,
                name: name.to_owned(),
            }),
        })
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
