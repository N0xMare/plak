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

/// Waits for this attachment's profile verdict: the move to its successor or
/// the refusal's reason.
fn profile_moved(client: &mut Client) -> Result<NodeAddress, String> {
    use plakd_wire_types::SurfaceObservation;
    loop {
        match client.next() {
            HostFrame::Moved { node, .. } => return Ok(node),
            HostFrame::Event {
                observation:
                    plakous_host_protocol::SessionObservation::Extension(
                        SurfaceObservation::ProfileRefused { reason, .. }
                        | SurfaceObservation::AccountChoiceRefused(
                            plakd_wire_types::AccountChoiceRefusal::Failed { reason },
                        ),
                    ),
                ..
            } => return Err(reason),
            _ => {}
        }
    }
}

/// A named root through the host (spec 02 §4.4.1): `new_session` with an
/// explicit named root account binds and runs on it, and `ApplyProfile` with
/// another named account switches the root's line onto that account, whose
/// successor runs as well.
#[test]
fn a_named_root_creates_runs_and_switches_through_the_host() {
    use plakd_wire_types::{ProfileApply, SurfaceIntent};
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": ANSWER}}]
    }));
    template(&fixture, "many", "codex", "offline-loopback-v1", "medium");
    // The offline route binds every codex account alike, so the switch also
    // moves the effort for the runtime configuration to change.
    template(&fixture, "many-low", "codex", "offline-loopback-v1", "low");
    let profiles = fixture.home().join("profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    write_private(
        &profiles.join("many-low.toml"),
        "root = \"many-low\"\nworking_set = [\"many\"]\n",
    );
    let mut host = start_env(
        &fixture,
        &[],
        &[("PLAK_TEST_READY_ACCOUNTS", "codex/a,codex/b".to_owned())],
    );
    let mut client = Client::connect(&host);
    let created = create(&mut client, &fixture, 1, "many", &[("codex", "codex/a")]);
    assert_eq!(created_account(&fixture, &created), "codex/a");
    let Outcome::Ok(Answer::Created { session, node, .. }) = created else {
        unreachable!("created above");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "on the named root", ANSWER);
    profiles::wait_idle(&mut client, &session);

    client.retain_observations();
    assert_eq!(
        client.request(
            4,
            Op::Intent {
                client_id: None,
                attachment,
                intent: SessionIntent::Extension(SurfaceIntent::ApplyProfile {
                    name: "many-low".into(),
                    apply: ProfileApply::Switch,
                    accounts: vec![plakd_wire_types::AccountChoice {
                        kind: plakd_wire_types::console::Vertical::Codex,
                        account: "codex/b".into(),
                    }],
                }),
                blobs: Vec::new(),
            },
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let moved = profile_moved(&mut client).expect("the switch onto codex/b");
    assert_eq!(moved, node, "a switch keeps the root's node");
    let rows = client.rows(5);
    let successor = rows
        .iter()
        .find(|row| row.session != session && row.node == node)
        .expect("the switch's successor is listed")
        .session
        .clone();
    let store = SqliteSessionStore::new(plakd::home::paths::PlakPaths::explicit(fixture.home()));
    let switched = store.load(&successor).unwrap();
    assert_eq!(switched.account.as_deref(), Some("codex/b"));
    assert_eq!(switched.effort.as_deref(), Some("low"));
    let attachment = client.attach(6, &node);
    client.prompt(7, attachment, "on the switched root", ANSWER);
    profiles::wait_idle(&mut client, &successor);
    assert_eq!(
        store.load(&session).unwrap().account.as_deref(),
        Some("codex/a"),
        "the source keeps its account"
    );
    drop(client);
    stop(&mut host);
}
