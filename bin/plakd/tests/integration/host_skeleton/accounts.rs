//! Session account choice through host `new_session` (spec 02 §4.4.1): the
//! fill rule over 0, 1 and n Ready accounts, a Ready default slot, an
//! explicit choice, each refusal, and a replay that compares accounts.

use super::*;
use plakd::store::session_records::{SessionRecordStore as _, SqliteSessionStore};
use plakous_host_protocol::wire::{AccountChoice, KindAccounts, RequestRefusal};

fn template(fixture: &Fixture, name: &str, account: &str, model: &str, effort: &str) {
    write_private(
        &fixture.home().join("agents").join(format!("{name}.toml")),
        &format!(
            "account = \"{account}\"\nmodel = \"{model}\"\neffort = \"{effort}\"\ncomposition = \"direct\"\ndescription = \"{name}\"\n"
        ),
    );
}

fn create(
    client: &mut Client,
    fixture: &Fixture,
    id: u64,
    template: &str,
    accounts: &[(&str, &str)],
) -> Outcome<PlakExtension> {
    client.request(
        id,
        Op::NewSession {
            runner: plakous_host_protocol::wire::RunnerLabel::local(),
            profile: None,
            model: None,
            workspace: fixture.workspace().display().to_string(),
            template: Some(template.to_owned()),
            request_id: format!("00000000-0000-4000-8000-{id:012}"),
            accounts: accounts
                .iter()
                .map(|(kind, account)| AccountChoice {
                    kind: (*kind).to_owned(),
                    account: (*account).to_owned(),
                })
                .collect(),
        },
    )
}

fn created_account(fixture: &Fixture, outcome: &Outcome<PlakExtension>) -> String {
    let Outcome::Ok(Answer::Created { session, .. }) = outcome else {
        panic!("expected a created session: {outcome:?}");
    };
    SqliteSessionStore::new(plakd::home::paths::PlakPaths::explicit(fixture.home()))
        .load(session)
        .unwrap()
        .account
        .unwrap()
}

fn required(outcome: &Outcome<PlakExtension>) -> Vec<KindAccounts> {
    let Outcome::Refused(RequestRefusal::AccountChoiceRequired { kinds }) = outcome else {
        panic!("expected account_choice_required: {outcome:?}");
    };
    kinds.clone()
}

fn invalid_reason(outcome: &Outcome<PlakExtension>) -> String {
    let Outcome::Refused(RequestRefusal::AccountChoiceInvalid { reason, .. }) = outcome else {
        panic!("expected account_choice_invalid: {outcome:?}");
    };
    reason.clone()
}

#[test]
fn new_session_fills_each_used_kind_before_the_root_resolves() {
    let Some(fixture) = fixture() else { return };
    // An API kind's model is checked at load; the subscription kinds bind it.
    template(&fixture, "none", "openai", "gpt-6-luna", "none");
    for (name, account) in [("one", "claude"), ("many", "codex"), ("fallback", "grok")] {
        template(&fixture, name, account, "offline-loopback-v1", "medium");
    }
    let mut host = start_env(
        &fixture,
        &[],
        &[(
            "PLAK_TEST_READY_ACCOUNTS",
            "claude/work,codex/a,codex/b,grok,grok/alt".to_owned(),
        )],
    );
    let mut client = Client::connect(&host);

    // No Ready account of the kind: refused, listing the kind.
    let kinds = required(&create(&mut client, &fixture, 1, "none", &[]));
    assert_eq!(kinds.len(), 1);
    assert_eq!((kinds[0].kind.as_str(), kinds[0].rows.len()), ("openai", 0));
    // Exactly one Ready account: it is the root's.
    let one = create(&mut client, &fixture, 2, "one", &[]);
    assert_eq!(created_account(&fixture, &one), "claude/work");
    // Several Ready and no default slot: refused with every choice listed.
    let kinds = required(&create(&mut client, &fixture, 3, "many", &[]));
    assert_eq!(
        kinds[0]
            .rows
            .iter()
            .map(|row| (row.account.as_str(), row.ready))
            .collect::<Vec<_>>(),
        [("codex/a", true), ("codex/b", true)]
    );
    // A Ready default slot wins over the named accounts.
    let default = create(&mut client, &fixture, 4, "fallback", &[]);
    assert_eq!(created_account(&fixture, &default), "grok");
    // An explicit choice wins.
    let explicit = create(&mut client, &fixture, 5, "many", &[("codex", "codex/b")]);
    assert_eq!(created_account(&fixture, &explicit), "codex/b");
    // Each invalid explicit choice is refused before anything is created.
    for (id, choice, reason) in [
        (6, ("codex", "claude/work"), "not an account of this kind"),
        (7, ("codex", "codex/c"), "not ready"),
        (
            8,
            ("claude", "claude/work"),
            "kind is not used by this profile",
        ),
        (9, ("nope", "codex/a"), "not a provider kind"),
    ] {
        assert_eq!(
            invalid_reason(&create(&mut client, &fixture, id, "many", &[choice])),
            reason
        );
    }
    // A replay compares the requested accounts with the stored ones.
    let request = |accounts: &[(&str, &str)]| Op::NewSession {
        runner: plakous_host_protocol::wire::RunnerLabel::local(),
        profile: None,
        model: None,
        workspace: fixture.workspace().display().to_string(),
        template: Some("many".to_owned()),
        request_id: format!("00000000-0000-4000-8000-{:012}", 5),
        accounts: accounts
            .iter()
            .map(|(kind, account)| AccountChoice {
                kind: (*kind).to_owned(),
                account: (*account).to_owned(),
            })
            .collect(),
    };
    assert_eq!(
        client.request(10, request(&[("codex", "codex/b")])),
        explicit
    );
    assert!(matches!(
        client.request(11, request(&[("codex", "codex/a")])),
        Outcome::Refused(RequestRefusal::Malformed { .. })
    ));
    stop(&mut host);
}
