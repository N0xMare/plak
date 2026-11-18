use super::*;

fn ready(account: &str) -> ReadyAccount {
    ReadyAccount {
        account: account.to_owned(),
        ready: true,
        label: None,
    }
}

fn unready(account: &str) -> ReadyAccount {
    ReadyAccount {
        account: account.to_owned(),
        ready: false,
        label: None,
    }
}

fn kinds(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn choice(kind: &str, account: &str) -> AccountChoice {
    AccountChoice {
        kind: kind.to_owned(),
        account: account.to_owned(),
    }
}

#[test]
fn fill_takes_the_single_ready_account_and_refuses_none_or_many() {
    let used = kinds(&["codex", "claude"]);
    let Err(RequestRefusal::AccountChoiceRequired { kinds: required }) =
        fill(&used, Some("codex"), &[], &[], FillMode::Require)
    else {
        panic!("zero ready accounts must be refused");
    };
    assert_eq!(
        required
            .iter()
            .map(|row| row.kind.as_str())
            .collect::<Vec<_>>(),
        ["claude", "codex"]
    );
    let one = fill(
        &used,
        Some("codex"),
        &[],
        &[ready("codex/work"), ready("claude/alt")],
        FillMode::Require,
    )
    .unwrap();
    assert_eq!(one.root_account.as_deref(), Some("codex/work"));
    assert_eq!(
        one.choices,
        BTreeMap::from([("claude".to_owned(), "claude/alt".to_owned())])
    );
    let many = fill(
        &kinds(&["codex"]),
        Some("codex"),
        &[],
        &[ready("codex/a"), ready("codex/b"), unready("codex")],
        FillMode::Require,
    );
    let Err(RequestRefusal::AccountChoiceRequired { kinds: required }) = many else {
        panic!("two ready accounts need a choice");
    };
    assert_eq!(required[0].rows.len(), 3, "the refusal lists the choices");
}

#[test]
fn a_ready_default_slot_wins_over_named_accounts_and_an_explicit_choice_wins_over_both() {
    let catalog = [ready("codex"), ready("codex/work")];
    let bare = fill(
        &kinds(&["codex"]),
        Some("codex"),
        &[],
        &catalog,
        FillMode::Require,
    )
    .unwrap();
    assert_eq!(bare.root_account.as_deref(), Some("codex"));
    let explicit = fill(
        &kinds(&["codex"]),
        Some("codex"),
        &[choice("codex", "codex/work")],
        &catalog,
        FillMode::Require,
    )
    .unwrap();
    assert_eq!(explicit.root_account.as_deref(), Some("codex/work"));
    assert!(explicit.choices.is_empty(), "the root kind is never stored");
}

#[test]
fn invalid_explicit_choices_are_refused_with_their_reason() {
    let catalog = [ready("codex/work"), unready("codex/idle"), ready("claude")];
    for (choice, reason) in [
        (choice("codex", "claude"), "not an account of this kind"),
        (choice("codex", "codex/idle"), "not ready"),
        (choice("nope", "codex/work"), "not a provider kind"),
        (
            choice("claude", "claude"),
            "kind is not used by this profile",
        ),
    ] {
        let refused = fill(
            &kinds(&["codex"]),
            Some("codex"),
            std::slice::from_ref(&choice),
            &catalog,
            FillMode::Require,
        );
        let Err(RequestRefusal::AccountChoiceInvalid { reason: got, .. }) = refused else {
            panic!("{choice:?} must be refused");
        };
        assert_eq!(got, reason);
    }
    let repeated = fill(
        &kinds(&["codex"]),
        Some("codex"),
        &[choice("codex", "codex/work"), choice("codex", "codex/work")],
        &catalog,
        FillMode::Require,
    );
    assert!(matches!(
        repeated,
        Err(RequestRefusal::AccountChoiceInvalid { reason, .. }) if reason == "repeated choice"
    ));
}

#[test]
fn import_fill_never_refuses_and_keeps_the_imported_root() {
    let mut record = SessionRecord::new(std::path::PathBuf::from("/tmp/plak-choice"));
    record.account = Some("codex/old".into());
    fill_imported(
        &mut record,
        &kinds(&["codex", "claude", "grok"]),
        &[
            ready("codex/new"),
            ready("claude/alt"),
            ready("grok/a"),
            ready("grok/b"),
        ],
    );
    assert_eq!(record.account.as_deref(), Some("codex/old"));
    assert_eq!(
        record.account_choices,
        BTreeMap::from([("claude".to_owned(), "claude/alt".to_owned())])
    );
}

#[test]
fn the_cli_flag_names_a_kind_and_an_account_of_it() {
    let error = parse_kind_choice("codex").unwrap_err();
    assert!(error.contains("<kind>=<ref>"), "{error}");
    assert_eq!(
        parse_kind_choice("codex=codex/work").unwrap(),
        choice("codex", "codex/work")
    );
    assert!(parse_kind_choice("codex=claude/work").is_err());
    let (bare, choices) =
        split_account_flags(vec!["codex/work".into(), "claude=claude/alt".into()]).unwrap();
    assert_eq!(bare.as_deref(), Some("codex/work"));
    assert_eq!(choices, [choice("claude", "claude/alt")]);
    assert!(split_account_flags(vec!["codex".into(), "claude".into()]).is_err());
}

#[test]
fn reported_choices_put_the_derived_root_kind_first() {
    let stored = BTreeMap::from([
        ("claude".to_owned(), "claude/alt".to_owned()),
        ("anthropic".to_owned(), "anthropic".to_owned()),
    ]);
    assert_eq!(
        session_choices(Some("codex/work"), &stored),
        [
            ("codex".to_owned(), "codex/work".to_owned()),
            ("anthropic".to_owned(), "anthropic".to_owned()),
            ("claude".to_owned(), "claude/alt".to_owned()),
        ]
    );
}

#[test]
fn a_spawn_resolves_the_root_kind_from_the_line_head_and_others_from_the_map() {
    let mut session = SessionRecord::new(std::path::PathBuf::from("/tmp/plak-choice"));
    session.account = Some("codex/work".into());
    session
        .account_choices
        .insert("claude".into(), "claude/alt".into());
    assert_eq!(
        session.chosen_account(Vertical::Codex).as_deref(),
        Some("codex/work")
    );
    assert_eq!(
        session.chosen_account(Vertical::Claude).as_deref(),
        Some("claude/alt")
    );
    assert_eq!(session.chosen_account(Vertical::Grok), None);
}
