//! Protocol 12 account-choice intents and observations.

use plakd_wire_types::{ProfileApply, SurfaceIntent, SurfaceObservation};
use plakous_host_protocol::wire::AccountChoice;

#[test]
fn choose_account_and_apply_profile_choices_round_trip() {
    let choose = SurfaceIntent::ChooseAccount {
        kind: "kind".to_owned(),
        account: "kind/work".to_owned(),
        continue_failed: true,
    };
    let choose_spelling =
        r#"{"choose_account":{"kind":"kind","account":"kind/work","continue_failed":true}}"#;
    assert_eq!(serde_json::to_string(&choose).unwrap(), choose_spelling);
    assert_eq!(
        serde_json::from_str::<SurfaceIntent>(choose_spelling).unwrap(),
        choose
    );

    let apply = SurfaceIntent::ApplyProfile {
        name: "coder".to_owned(),
        apply: ProfileApply::Fork,
        accounts: vec![AccountChoice {
            kind: "kind".to_owned(),
            account: "kind/work".to_owned(),
        }],
    };
    let apply_spelling = r#"{"apply_profile":{"name":"coder","apply":"fork","accounts":[{"kind":"kind","account":"kind/work"}]}}"#;
    assert_eq!(serde_json::to_string(&apply).unwrap(), apply_spelling);
    let omitted = r#"{"apply_profile":{"name":"coder","apply":"fork"}}"#;
    assert_eq!(
        serde_json::from_str::<SurfaceIntent>(omitted).unwrap(),
        SurfaceIntent::ApplyProfile {
            name: "coder".to_owned(),
            apply: ProfileApply::Fork,
            accounts: Vec::new(),
        }
    );

    let observed = SurfaceObservation::AccountChoices {
        choices: vec![AccountChoice {
            kind: "kind".to_owned(),
            account: "kind/work".to_owned(),
        }],
    };
    let observed_spelling =
        r#"{"account_choices":{"choices":[{"kind":"kind","account":"kind/work"}]}}"#;
    assert_eq!(serde_json::to_string(&observed).unwrap(), observed_spelling);
    assert_eq!(
        serde_json::from_str::<SurfaceObservation>(observed_spelling).unwrap(),
        observed
    );
}
