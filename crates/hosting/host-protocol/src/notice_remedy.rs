//! Semantic operator remedies, without frontend commands or presentation text.

use serde::{Deserialize, Serialize};

/// Account whose native login needs attention; unspecified is an explicit placeholder.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountTarget {
    /// Codex native account.
    Codex,
    /// Grok native account.
    Grok,
    /// Claude native account.
    Claude,
    /// `OpenRouter` account.
    OpenRouter,
    /// No account identity was available at the failure site.
    Unspecified,
}

impl AccountTarget {
    /// Preserve a known native account identity, otherwise an explicit unknown target.
    #[must_use]
    pub fn from_account(account: &str) -> Self {
        match account {
            "codex" => Self::Codex,
            "grok" => Self::Grok,
            "claude" => Self::Claude,
            "openrouter" => Self::OpenRouter,
            _ => Self::Unspecified,
        }
    }
}

/// Why a saved credential cannot be used.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialCondition {
    /// A previous profile/build wrote the login.
    OlderProfile,
    /// A newer build wrote the login.
    NewerBuild,
    /// The login was rejected or quarantined.
    Rejected,
    /// No usable login was available.
    Unavailable,
}

/// Exact saved product session identity, validated as a UUID on decode.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProductSessionId(uuid::Uuid);

impl std::str::FromStr for ProductSessionId {
    type Err = uuid::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

impl std::fmt::Display for ProductSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One semantic remedy; compound cases name alternatives, never arbitrary command lists.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case", deny_unknown_fields)]
pub enum NoticeRemedy {
    /// child-recover.
    RecoverChildren,
    /// binding.
    Rebind,
    /// login.
    Login {
        /// Original account identity.
        account: AccountTarget,
    },
    /// retry, then login.
    RetryThenLogin {
        /// Original account identity.
        account: AccountTarget,
    },
    /// processes.
    StopProcesses,
    /// attach.
    Attach,
    /// scheduled.
    ResumeQueueOrStartFresh,
    /// fork-target.
    ResumeSavedSession {
        /// Exact saved target session.
        session_id: ProductSessionId,
    },
    /// compact.
    RetryCompaction,
    /// same TUI qualification; no run action.
    AttachOnlyWhenDetached,
    /// tree.
    ResumeTree,
    /// fresh.
    StartFresh,
    /// profiles plus retry.
    RetryOrChooseProfile,
    /// recover.
    RecoverSession,
    /// cancel (Ctrl-C for run; Esc for TUI restore).
    InterruptWait,
    /// cancel.
    CancelContinuation,
    /// resume-picker.
    ResumeExactSession,
    /// profiles.
    ChooseProfile,
    /// profiles with explicit root effort.
    ChooseProfileEffort,
    /// profiles + attach.
    ChooseProfileOrAttach,
    /// exact env/sidecar placement, then attach.
    RepairJavaScriptHost,
    /// exact env/sidecar placement, then attach.
    RepairPythonHost,
    /// binding; /reload//rebind, same-login caveat.
    CarryCurrentLogin,
    /// binding; original account, not current account.
    RestoreAccountOrRebind {
        /// Original account identity.
        account: AccountTarget,
    },
    /// binding; full-history and frozen-binding caveats.
    RestoreBindingOrCarry {
        /// Original account identity.
        account: AccountTarget,
    },
    /// login then attach; same-account route qualification.
    LoginThenAttach {
        /// Original account identity.
        account: AccountTarget,
    },
    /// attach; profile selection is not repair.
    RepairThenAttach,
    /// condition-specific login/newer recipe, profile alternative.
    RepairCredentialOrChooseProfile {
        /// Original account identity.
        account: AccountTarget,
        /// Credential condition at refusal.
        condition: CredentialCondition,
    },
    /// capacity; preserve whole-group/no-fit qualification.
    CompactWholeGroups,
    /// capacity; preserve latest-user-message protection/no-fit qualification.
    CompactNative,
    /// segments.
    CompactSegmentHistory,
    /// protected.
    LargerModelOrStartFresh,
    /// TUI /fork; CLI opens TUI; JSONL keeps current text.
    RetryFork,
    /// reserve.
    ReclaimThenRecover,
    /// retention.
    ReclaimRetainedHistory,
    /// disk.
    FreeDiskSpace,
}
