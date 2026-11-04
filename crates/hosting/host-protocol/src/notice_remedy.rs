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
            "codex" | "chatgpt" => Self::Codex,
            // `xai` is NativeProvider::as_str for the SuperGrok subscription.
            // `supergrok` and `xai-oauth` are that subscription's product aliases.
            "grok" | "supergrok" | "xai" | "xai-oauth" => Self::Grok,
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

impl ProductSessionId {
    /// Preserve an already-validated session UUID without text parsing.
    #[must_use]
    pub const fn from_uuid(id: uuid::Uuid) -> Self {
        Self(id)
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
    /// Recover unresolved child sessions only.
    RecoverChildren {},
    /// Resume the agents a lost host interrupted, one from the Agents tab
    /// or every held one at once; carried only by live notices.
    ResumeAfterLoss {},
    /// Rebind or start over after the frozen binding changed.
    Rebind {},
    /// Restore a usable login for the account, then retry.
    Login {
        /// Original account identity.
        account: AccountTarget,
    },
    /// Retry once, then restore credentials if it persists.
    RetryThenLogin {
        /// Original account identity.
        account: AccountTarget,
    },
    /// Stop running processes before retrying the transition.
    StopProcesses {},
    /// Reopen the session so interrupted work can recover.
    Attach {},
    /// Resume the paused queue, retry compaction as a new paid operation, or start fresh.
    ResumeQueueOrStartFresh {},
    /// Open the exact saved target session.
    ResumeSavedSession {
        /// Exact saved target session.
        session_id: ProductSessionId,
    },
    /// Retry compaction; a retry is a new paid operation.
    RetryCompaction {},
    /// Already attached; reopening applies only when detached.
    AttachOnlyWhenDetached {},
    /// Retained spelling of the removed tree resume. Stored notice rows and
    /// previous-version exports still carry it; no frontend renders an action.
    ResumeTree {},
    /// Start a new conversation instead of replaying the failed turn.
    StartFresh {},
    /// Retry, or choose another admitted profile.
    RetryOrChooseProfile {},
    /// Retry session recovery.
    RecoverSession {},
    /// Cancel the wait or automatic continuation.
    InterruptWait {},
    /// Cancel the continuation.
    CancelContinuation {},
    /// Resume by an exact session identity.
    ResumeExactSession {},
    /// Choose an admitted profile.
    ChooseProfile {},
    /// Choose a profile whose root names an explicit effort.
    ChooseProfileEffort {},
    /// Choose a profile, or reopen after the failure is repaired.
    ChooseProfileOrAttach {},
    /// Place the JavaScript host where the product expects it, then reopen.
    RepairJavaScriptHost {},
    /// Place the Python host where the product expects it, then reopen.
    RepairPythonHost {},
    /// Carry history under the current login; the previous selection cannot be restored by logging in again.
    CarryCurrentLogin {},
    /// Restore the original account for an exact reopen, or carry history under the current login.
    RestoreAccountOrRebind {
        /// Original account identity.
        account: AccountTarget,
    },
    /// Restore the original binding, or carry the full history under the current login.
    RestoreBindingOrCarry {
        /// Original account identity.
        account: AccountTarget,
    },
    /// Restore the login, then reopen. A same-account login preserves the session only where the route attests it.
    LoginThenAttach {
        /// Original account identity.
        account: AccountTarget,
    },
    /// Repair the attach failure, then reopen. Choosing another profile is not that repair.
    RepairThenAttach {},
    /// Repair the credential for its condition, or choose another profile.
    RepairCredentialOrChooseProfile {
        /// Original account identity.
        account: AccountTarget,
        /// Credential condition at refusal.
        condition: CredentialCondition,
    },
    /// Compact this checkpoint; a fit of the retained projection is not guaranteed.
    CompactWholeGroups {},
    /// Compact this checkpoint under latest-user-message protection; a fit is not guaranteed.
    CompactNative {},
    /// Reduce this session, reduce a fork, or start fresh. Those are different operations.
    CompactSegmentHistory {},
    /// Choose a larger model or start fresh. The failed turn is not replayed.
    LargerModelOrStartFresh {},
    /// Retry the fork.
    RetryFork {},
    /// Reclaim session storage, then retry recovery.
    ReclaimThenRecover {},
    /// Reclaim retained history. Clearing, compacting, or recovering does not.
    ReclaimRetainedHistory {},
    /// Free disk space. The logical reserve does not reserve disk blocks.
    FreeDiskSpace {},
}

#[cfg(test)]
mod tests {
    use super::AccountTarget;

    #[test]
    fn every_capability_alias_maps_to_its_account() {
        let cases = [
            ("codex", AccountTarget::Codex),
            ("chatgpt", AccountTarget::Codex),
            ("grok", AccountTarget::Grok),
            ("supergrok", AccountTarget::Grok),
            ("xai", AccountTarget::Grok),
            ("xai-oauth", AccountTarget::Grok),
            ("claude", AccountTarget::Claude),
            ("openrouter", AccountTarget::OpenRouter),
            ("openai", AccountTarget::Unspecified),
            ("", AccountTarget::Unspecified),
        ];
        for (alias, expected) in cases {
            assert_eq!(AccountTarget::from_account(alias), expected, "{alias}");
        }
    }
}
