//! Credential-free login instructions and compatibility notices.

use super::{Duration, Instant, NativeProvider, fmt};

/// Read-only, process-local evidence retained while a pending login is consumed.
/// This is not credential or completion authority and cannot be reset by a caller.
#[derive(Clone, Debug)]
pub struct NativeLoginEffects(pub(super) std::sync::Arc<std::sync::atomic::AtomicBool>);

impl NativeLoginEffects {
    /// Whether completion entered potentially effectful recovery/provider I/O.
    /// False proves that a returned completion error preceded that boundary;
    /// true does not claim a credential was written.
    #[must_use]
    pub fn may_have_started(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Supported native login interaction modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLoginMode {
    /// Authorization-code-with-PKCE through a fixed loopback callback.
    Browser,
    /// The browser runs on another machine: Codex uses the device-code flow;
    /// xAI and Claude complete with the code their hosted page displays.
    Remote,
}

/// How one displayed native-login instruction is completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLoginCompletion {
    /// The provider loopback callback completes without more input.
    Automatic,
    /// The operator enters the displayed one-time code at the verification
    /// URL; completion polls the provider without more local input.
    DeviceCode,
    /// The operator may paste the displayed authorization code or final URL.
    AuthorizationCodeOrRedirectUrl,
}

/// Display-only instructions for one pending native login.
///
/// The authorization URL is deliberately accessible because the operator must
/// act on it. Debug output remains redacted.
pub struct NativeLoginInstructions<'a> {
    pub(super) verification_uri: &'a str,
    pub(super) completion: NativeLoginCompletion,
    pub(super) device_code: Option<NativeDeviceCode<'a>>,
}

/// The one-time code of a pending device-code login.
///
/// The code is short-lived and shown only to the operator who started the
/// login. Debug output is redacted.
#[derive(Clone, Copy)]
pub struct NativeDeviceCode<'a> {
    pub(super) user_code: &'a str,
    pub(super) deadline: Instant,
}

impl NativeDeviceCode<'_> {
    /// Returns the code the operator enters at the verification URL.
    #[must_use]
    pub const fn user_code(&self) -> &str {
        self.user_code
    }

    /// Returns how long the code stays valid from now.
    #[must_use]
    pub fn expires_in(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

impl fmt::Debug for NativeDeviceCode<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeDeviceCode(<redacted one-time code>)")
    }
}

impl NativeLoginInstructions<'_> {
    /// Returns the exact provider URL the operator should open.
    #[must_use]
    pub const fn verification_uri(&self) -> &str {
        self.verification_uri
    }

    /// Returns whether completion is automatic, polls a device code, or needs
    /// pasted input.
    #[must_use]
    pub const fn completion(&self) -> NativeLoginCompletion {
        self.completion
    }

    /// Returns the one-time code when completion is
    /// [`NativeLoginCompletion::DeviceCode`].
    #[must_use]
    pub const fn device_code(&self) -> Option<NativeDeviceCode<'_>> {
        self.device_code
    }
}

impl fmt::Debug for NativeLoginInstructions<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeLoginInstructions(<redacted operator instructions>)")
    }
}

/// Immutable, credential-free facts requiring explicit compatibility consent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompatibilityNotice {
    pub(super) provider: NativeProvider,
    pub(super) profile_id: &'static str,
    pub(super) source_revision: &'static str,
    pub(super) secondary_source_revision: Option<&'static str>,
    pub(super) notice_revision: &'static str,
    pub(super) provenance: &'static str,
    pub(super) reviewed_at: u64,
    pub(super) expires_at: u64,
    pub(super) kill_switch_revision: &'static str,
    pub(super) killed: bool,
}

impl CompatibilityNotice {
    /// Returns the integration this notice governs.
    #[must_use]
    pub const fn provider(self) -> NativeProvider {
        self.provider
    }

    /// Returns the exact compatibility profile identity.
    #[must_use]
    pub const fn profile_id(self) -> &'static str {
        self.profile_id
    }

    /// Returns the immutable reviewed source revision.
    #[must_use]
    pub const fn source_revision(self) -> &'static str {
        self.source_revision
    }

    /// Returns an additional immutable official source revision when the
    /// integration joins two independently reviewed sources.
    #[must_use]
    pub const fn secondary_source_revision(self) -> Option<&'static str> {
        self.secondary_source_revision
    }

    /// Returns the exact text the operator must enter to consent.
    #[must_use]
    pub const fn notice_revision(self) -> &'static str {
        self.notice_revision
    }

    /// Returns the compatibility provenance label.
    #[must_use]
    pub const fn provenance(self) -> &'static str {
        self.provenance
    }

    /// Returns the Unix timestamp of the compatibility review.
    #[must_use]
    pub const fn reviewed_at(self) -> u64 {
        self.reviewed_at
    }

    /// Returns the hard Unix expiry of this compatibility profile.
    #[must_use]
    pub const fn expires_at(self) -> u64 {
        self.expires_at
    }

    /// Returns the local kill-switch revision.
    #[must_use]
    pub const fn kill_switch_revision(self) -> &'static str {
        self.kill_switch_revision
    }

    /// Reports whether this build has disabled the profile.
    #[must_use]
    pub const fn is_killed(self) -> bool {
        self.killed
    }
}
