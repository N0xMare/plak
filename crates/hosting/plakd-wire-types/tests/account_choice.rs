//! Protocol 12 account-choice intents and observations.

#![allow(clippy::unwrap_used, reason = "contract-owned codec values")]

use plakd_wire_types::console::Vertical;
use plakd_wire_types::{
    AccountChoice, AccountChoiceRefusal, ProfileApply, SurfaceIntent, SurfaceObservation,
};

fn round_trips<T>(value: &T, spelling: &str)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_string(value).unwrap(), spelling);
    assert_eq!(&serde_json::from_str::<T>(spelling).unwrap(), value);
}

#[test]
fn choose_account_and_apply_profile_choices_round_trip() {
    round_trips(
        &SurfaceIntent::ChooseAccount {
            kind: Vertical::Codex,
            account: "codex/work".to_owned(),
            continue_failed: true,
        },
        r#"{"choose_account":{"kind":"codex","account":"codex/work","continue_failed":true}}"#,
    );
    round_trips(
        &SurfaceIntent::ApplyProfile {
            name: "coder".to_owned(),
            apply: ProfileApply::Fork,
            accounts: vec![AccountChoice {
                kind: Vertical::XaiApi,
                account: "xai-api/work".to_owned(),
            }],
        },
        r#"{"apply_profile":{"name":"coder","apply":"fork","accounts":[{"kind":"xai-api","account":"xai-api/work"}]}}"#,
    );
    assert_eq!(
        serde_json::from_str::<SurfaceIntent>(
            r#"{"apply_profile":{"name":"coder","apply":"fork"}}"#
        )
        .unwrap(),
        SurfaceIntent::ApplyProfile {
            name: "coder".to_owned(),
            apply: ProfileApply::Fork,
            accounts: Vec::new(),
        }
    );
    assert!(
        serde_json::from_str::<SurfaceIntent>(
            r#"{"choose_account":{"kind":"codex/work","account":"codex/work"}}"#
        )
        .is_err(),
        "a named account is not a kind"
    );
}

#[test]
fn account_choices_and_their_refusals_have_closed_spellings() {
    round_trips(
        &SurfaceObservation::AccountChoices {
            choices: vec![AccountChoice {
                kind: Vertical::Codex,
                account: "codex/work".to_owned(),
            }],
        },
        r#"{"account_choices":{"choices":[{"kind":"codex","account":"codex/work"}]}}"#,
    );
    round_trips(
        &SurfaceObservation::AccountChoiceRefused(AccountChoiceRefusal::RootBusy),
        r#"{"account_choice_refused":{"refusal":"root_busy"}}"#,
    );
    round_trips(
        &SurfaceObservation::AccountChoiceRefused(AccountChoiceRefusal::AccountChoiceInvalid {
            kind: "codex".to_owned(),
            account: "codex/idle".to_owned(),
            reason: "not ready".to_owned(),
        }),
        r#"{"account_choice_refused":{"refusal":"account_choice_invalid","kind":"codex","account":"codex/idle","reason":"not ready"}}"#,
    );
    for (refusal, spelling) in [
        (AccountChoiceRefusal::NoFailedTurn, "no_failed_turn"),
        (AccountChoiceRefusal::AlreadyContinued, "already_continued"),
    ] {
        round_trips(
            &SurfaceObservation::AccountChoiceRefused(refusal),
            &format!(r#"{{"account_choice_refused":{{"refusal":"{spelling}"}}}}"#),
        );
    }
}
