//! Plak-owned credential and inference-key guidance for the shared Usage facts.

use plakous_host_protocol::views::{KeySource, UsageCredentialState, UsageRepair};

pub(crate) fn unavailable_reason(
    repair: Option<UsageRepair>,
    account: &str,
    neutral: &str,
) -> String {
    let Some(repair) = repair else {
        return neutral.to_owned();
    };
    match repair {
        UsageRepair::Credential(UsageCredentialState::RefreshRequired) => format!("credential needs a token refresh; the next turn refreshes it, or run `plak refresh {account}`"),
        UsageRepair::Credential(UsageCredentialState::Quarantined) if account == "grok" => "credential is quarantined after an ambiguous refresh; the next turn retries it, or run `plak refresh grok`; run `plak login grok` if that fails".into(),
        UsageRepair::Credential(UsageCredentialState::Quarantined) => format!("credential is quarantined after an ambiguous refresh; run `plak login {account}` to recover"),
        UsageRepair::Credential(UsageCredentialState::Revoked) => format!("credential is revoked; run `plak login {account}`"),
        UsageRepair::Credential(UsageCredentialState::LoginRequired) => format!("the stored login was made by an earlier build and cannot be used; run `plak login {account}`"),
        UsageRepair::Credential(UsageCredentialState::WrittenByNewerBuild) => format!("the stored login was written by a newer plak build; use that build, or replace it with `plak login {account}` (that build must then log in again)"),
        UsageRepair::RejectedKey(KeySource::File) => format!("{neutral}; run `plak key set openrouter` to replace it"),
        UsageRepair::RejectedKey(KeySource::Environment) => neutral.to_owned(),
    }
}

pub(crate) fn not_connected(account: &str) -> String {
    let command = if account == "openrouter" {
        "plak key set openrouter".to_owned()
    } else {
        format!("plak login {account}")
    };
    format!("not connected · run `{command}`")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_and_key_guidance_uses_typed_state_and_original_source() {
        for state in [
            UsageCredentialState::RefreshRequired,
            UsageCredentialState::Quarantined,
            UsageCredentialState::Revoked,
            UsageCredentialState::LoginRequired,
            UsageCredentialState::WrittenByNewerBuild,
        ] {
            let text = unavailable_reason(Some(UsageRepair::Credential(state)), "grok", "opaque");
            assert!(text.contains("plak refresh grok") || text.contains("plak login grok"));
        }
        assert!(
            unavailable_reason(
                Some(UsageRepair::Credential(
                    UsageCredentialState::WrittenByNewerBuild
                )),
                "claude",
                "opaque"
            )
            .contains("that build must then log in again")
        );
        assert!(
            unavailable_reason(
                Some(UsageRepair::RejectedKey(KeySource::File)),
                "openrouter",
                "rejected"
            )
            .contains("plak key set openrouter")
        );
        assert_eq!(
            unavailable_reason(
                Some(UsageRepair::RejectedKey(KeySource::Environment)),
                "openrouter",
                "replace OPENROUTER_API_KEY"
            ),
            "replace OPENROUTER_API_KEY"
        );
        assert_eq!(
            unavailable_reason(None, "grok", "legacy plak login codex"),
            "legacy plak login codex"
        );
    }
}
