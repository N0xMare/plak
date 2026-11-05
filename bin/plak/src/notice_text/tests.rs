use super::*;

pub(crate) fn all_remedies() -> Vec<NoticeRemedy> {
    use NoticeRemedy as R;
    vec![
        R::RecoverChildren {},
        R::Rebind {},
        R::Login {
            account: AccountTarget::Grok,
        },
        R::RetryThenLogin {
            account: AccountTarget::Grok,
        },
        R::StopProcesses {},
        R::Attach {},
        R::ResumeQueueOrStartFresh {},
        R::ResumeSavedSession {
            session_id: "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap(),
        },
        R::RetryCompaction {},
        R::AttachOnlyWhenDetached {},
        R::ResumeTree {},
        R::ResumeAfterLoss {},
        R::StartFresh {},
        R::RetryOrChooseProfile {},
        R::RecoverSession {},
        R::InterruptWait {},
        R::CancelContinuation {},
        R::ResumeExactSession {},
        R::ChooseProfile {},
        R::ChooseProfileEffort {},
        R::ChooseProfileOrAttach {},
        R::RepairJavaScriptHost {},
        R::RepairPythonHost {},
        R::CarryCurrentLogin {},
        R::RestoreAccountOrRebind {
            account: AccountTarget::Grok,
        },
        R::RestoreBindingOrCarry {
            account: AccountTarget::Grok,
        },
        R::LoginThenAttach {
            account: AccountTarget::Grok,
        },
        R::RepairThenAttach {},
        R::RepairCredentialOrChooseProfile {
            account: AccountTarget::Grok,
            condition: CredentialCondition::NewerBuild,
        },
        R::CompactWholeGroups {},
        R::CompactNative {},
        R::CompactSegmentHistory {},
        R::LargerModelOrStartFresh {},
        R::RetryFork {},
        R::ReclaimThenRecover {},
        R::ReclaimRetainedHistory {},
        R::FreeDiskSpace {},
    ]
}

#[test]
fn every_remedy_has_each_existing_frontend_route_and_strict_encoding() {
    assert_eq!(all_remedies().len(), 37);
    for remedy in all_remedies() {
        let value = serde_json::to_value(&remedy).unwrap();
        assert_eq!(
            serde_json::from_value::<NoticeRemedy>(value.clone()).unwrap(),
            remedy
        );
        assert!(
            value["code"]
                .as_str()
                .is_some_and(|code| !code.contains(char::is_uppercase))
        );
        let mut extra = value;
        extra["unexpected"] = true.into();
        assert!(serde_json::from_value::<NoticeRemedy>(extra).is_err());
        for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
            let rendered = with_remedy(
                "neutral facts",
                Some(&remedy),
                RemedyContext {
                    frontend,
                    session_id: Some("current-id"),
                },
            );
            assert!(rendered.starts_with("neutral facts"));
            let intentionally_absent = matches!(
                (&remedy, frontend),
                (
                    NoticeRemedy::ResumeExactSession {},
                    Frontend::Run | Frontend::Jsonl
                ) | (NoticeRemedy::AttachOnlyWhenDetached {}, Frontend::Run)
                    | (NoticeRemedy::ResumeTree {}, _)
            );
            assert_eq!(
                rendered == "neutral facts",
                intentionally_absent,
                "{remedy:?}/{frontend:?}"
            );
            assert!(
                !rendered.contains("run --compact")
                    && !rendered.contains("run --rebind")
                    && !rendered.contains("run --resume-queue"),
                "{rendered}"
            );
        }
    }
    assert!(serde_json::from_str::<NoticeRemedy>(r#"{"code":"unknown_remedy"}"#).is_err());
    assert!(
        serde_json::from_str::<NoticeRemedy>(
            r#"{"code":"resume_saved_session","session_id":"not-a-uuid"}"#
        )
        .is_err()
    );
}

#[test]
fn action_help_uses_the_exact_target_and_valid_frontend_commands() {
    let render = |remedy: &NoticeRemedy, frontend| {
        remedy_text(
            remedy,
            RemedyContext {
                frontend,
                session_id: Some("current-id"),
            },
        )
    };
    assert!(
        render(&NoticeRemedy::ResumeQueueOrStartFresh {}, Frontend::Jsonl).contains("resume_queue")
    );
    assert!(
        render(&NoticeRemedy::ResumeQueueOrStartFresh {}, Frontend::Tui).contains("Tab then r")
    );
    let saved = NoticeRemedy::ResumeSavedSession {
        session_id: "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap(),
    };
    for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
        let text = render(&saved, frontend);
        assert!(text.contains("00112233-4455-6677-8899-aabbccddeeff"));
        assert!(!text.contains("current-id"));
    }
    let original = NoticeRemedy::RestoreAccountOrRebind {
        account: AccountTarget::Grok,
    };
    assert!(render(&original, Frontend::Tui).contains("plak login grok"));
    let binding = NoticeRemedy::RestoreBindingOrCarry {
        account: AccountTarget::Claude,
    };
    let tui = render(&binding, Frontend::Tui);
    assert!(tui.starts_with("carry the full history into a new session with /rebind"));
    assert!(tui.contains("plak login claude"));
    for frontend in [Frontend::Run, Frontend::Jsonl] {
        assert!(render(&binding, frontend).contains("/rebind to carry the full history"));
    }
    let newer = NoticeRemedy::RepairCredentialOrChooseProfile {
        account: AccountTarget::Claude,
        condition: CredentialCondition::NewerBuild,
    };
    assert!(render(&newer, Frontend::Jsonl).contains("writing build must then log in again"));
    assert!(render(&newer, Frontend::Run).contains("plak login claude"));
}

#[test]
fn absent_remedy_is_verbatim_and_never_inferred_from_text() {
    for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
        assert_eq!(
            with_remedy(
                "legacy /recover text",
                None,
                RemedyContext {
                    frontend,
                    session_id: None
                }
            ),
            "legacy /recover text"
        );
    }
}

#[test]
fn live_restored_pause_names_the_queue_action_in_every_existing_face() {
    use plakous_host_protocol::status::HostPause;
    for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
        let text = restored_pause(
            HostPause { queue: true },
            RemedyContext {
                frontend,
                session_id: Some("exact-root"),
            },
        );
        let queue_action = match frontend {
            Frontend::Tui => "Tab",
            Frontend::Run => "plak --resume exact-root",
            Frontend::Jsonl => "resume_queue",
        };
        assert!(text.contains(queue_action), "{text}");
        assert!(
            text.contains("one item") || text.contains("one queued item"),
            "{text}"
        );
    }
}

/// Compile-time cover of every remedy. Adding a variant without an arm fails
/// this crate; the matrix below then has to render it.
fn remedies_are_exhaustive(remedy: &NoticeRemedy) {
    use NoticeRemedy as R;
    match remedy {
        R::RecoverChildren {}
        | R::Rebind {}
        | R::StopProcesses {}
        | R::Attach {}
        | R::ResumeQueueOrStartFresh {}
        | R::RetryCompaction {}
        | R::AttachOnlyWhenDetached {}
        | R::ResumeTree {}
        | R::ResumeAfterLoss {}
        | R::StartFresh {}
        | R::RetryOrChooseProfile {}
        | R::RecoverSession {}
        | R::InterruptWait {}
        | R::CancelContinuation {}
        | R::ResumeExactSession {}
        | R::ChooseProfile {}
        | R::ChooseProfileEffort {}
        | R::ChooseProfileOrAttach {}
        | R::RepairJavaScriptHost {}
        | R::RepairPythonHost {}
        | R::CarryCurrentLogin {}
        | R::RepairThenAttach {}
        | R::CompactWholeGroups {}
        | R::CompactNative {}
        | R::CompactSegmentHistory {}
        | R::LargerModelOrStartFresh {}
        | R::RetryFork {}
        | R::ReclaimThenRecover {}
        | R::ReclaimRetainedHistory {}
        | R::FreeDiskSpace {} => {}
        R::Login { account }
        | R::RetryThenLogin { account }
        | R::RestoreAccountOrRebind { account }
        | R::RestoreBindingOrCarry { account }
        | R::LoginThenAttach { account } => {
            let _ = account;
        }
        R::ResumeSavedSession { session_id } => {
            let _ = session_id;
        }
        R::RepairCredentialOrChooseProfile { account, condition } => {
            let _ = (account, condition);
        }
    }
}

fn accounts() -> [AccountTarget; 5] {
    [
        AccountTarget::Codex,
        AccountTarget::Grok,
        AccountTarget::Claude,
        AccountTarget::OpenRouter,
        AccountTarget::Unspecified,
    ]
}

fn credential_conditions() -> [CredentialCondition; 4] {
    [
        CredentialCondition::OlderProfile,
        CredentialCondition::NewerBuild,
        CredentialCondition::Rejected,
        CredentialCondition::Unavailable,
    ]
}

fn backtick_commands(text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        let body = rest[..end].trim();
        rest = &rest[end + 1..];
        if body == "plak" || body.starts_with("plak ") {
            commands.push(body.to_owned());
        }
    }
    commands
}

fn command_argv(command: &str) -> Vec<std::ffi::OsString> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in command.chars() {
        match ch {
            '\'' => quoted = !quoted,
            ch if ch.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    argv.push(std::ffi::OsString::from(std::mem::take(&mut current)));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        argv.push(std::ffi::OsString::from(current));
    }
    argv
}

fn parse_rendered_command(command: &str) {
    let session = "00112233-4455-6677-8899-aabbccddeeff";
    let filled = command
        .replace("<prompt>", "ping")
        .replace("<id>", session)
        .replace("<account>", "codex")
        .replace("<session>", session)
        .replace("<file>", "carry.json");
    let mut argv = command_argv(&filled);
    assert_eq!(
        argv.first().and_then(|token| token.to_str()),
        Some("plak"),
        "{command}"
    );
    argv.remove(0);
    crate::cli::parse(argv).unwrap_or_else(|error| panic!("{command}: {error}"));
}

#[test]
fn every_account_and_credential_renders_a_command_the_parser_accepts() {
    let session = "00112233-4455-6677-8899-aabbccddeeff";
    let mut rendered_commands = 0_usize;
    let mut remedies = all_remedies();
    for account in accounts() {
        remedies.push(NoticeRemedy::Login { account });
        remedies.push(NoticeRemedy::RetryThenLogin { account });
        remedies.push(NoticeRemedy::RestoreAccountOrRebind { account });
        remedies.push(NoticeRemedy::RestoreBindingOrCarry { account });
        remedies.push(NoticeRemedy::LoginThenAttach { account });
        for condition in credential_conditions() {
            remedies.push(NoticeRemedy::RepairCredentialOrChooseProfile { account, condition });
        }
    }
    for remedy in &remedies {
        remedies_are_exhaustive(remedy);
        for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
            let text = remedy_text(
                remedy,
                RemedyContext {
                    frontend,
                    session_id: Some(session),
                },
            );
            if matches!(
                remedy,
                NoticeRemedy::Login {
                    account: AccountTarget::OpenRouter
                }
            ) {
                assert!(text.contains("plak key set openrouter"), "{text}");
                assert!(!text.contains("plak login openrouter"), "{text}");
            }
            if matches!(
                remedy,
                NoticeRemedy::Login {
                    account: AccountTarget::Unspecified
                }
            ) {
                assert!(text.contains("plak login <account>"), "{text}");
            }
            for command in backtick_commands(&text) {
                parse_rendered_command(&command);
                rendered_commands += 1;
            }
        }
    }
    assert!(rendered_commands > 0);
}

#[test]
fn advice_and_login_wait_commands_parse() {
    use plakous_host_protocol::refusal::RefusalAdvice;
    use plakous_host_protocol::views::ChildProgress;
    let session = "00112233-4455-6677-8899-aabbccddeeff";
    let advice = [
        RefusalAdvice::CheckStorage,
        RefusalAdvice::SecretMissing {
            server: "notes".into(),
            name: "token".into(),
        },
        RefusalAdvice::UnknownModel {
            model: "missing".into(),
            account: "codex".into(),
            catalog: "codex".into(),
        },
        RefusalAdvice::McpLogin {
            server: "notes".into(),
            scopes: vec!["mcp:tools".into(), "offline_access".into()],
        },
        RefusalAdvice::ExportSession {
            session_id: session.parse().unwrap(),
        },
    ];
    let mut commands = Vec::new();
    for advice in &advice {
        let text = crate::refusal_text::advice_text(advice, Frontend::Run);
        let found = backtick_commands(&text);
        assert!(!found.is_empty(), "{text}");
        commands.extend(found);
    }
    let progress = crate::refusal_text::child_progress(ChildProgress {
        credential_wait: Some("codex".into()),
        ..ChildProgress::default()
    });
    commands.extend(backtick_commands(
        progress.activity.as_deref().unwrap_or(""),
    ));
    assert!(
        commands
            .iter()
            .any(|command| command.contains("plak doctor"))
    );
    assert!(
        commands
            .iter()
            .any(|command| command.contains("plak secret set"))
    );
    assert!(
        commands
            .iter()
            .any(|command| command.contains("plak models"))
    );
    assert!(commands.iter().any(|command| command.contains("--scope")));
    assert!(
        commands
            .iter()
            .any(|command| command.contains("session export"))
    );
    assert!(
        commands
            .iter()
            .any(|command| command.contains("session import"))
    );
    assert!(
        commands
            .iter()
            .any(|command| command.contains("plak login codex"))
    );
    for command in commands {
        parse_rendered_command(&command);
    }
}

#[test]
fn store_capacity_remedies_name_the_store_file_and_the_reclaim_commands_in_order() {
    let store = crate::home::paths::PlakPaths::home_from_env()
        .unwrap()
        .runtime_database();
    for remedy in [
        NoticeRemedy::ReclaimRetainedHistory {},
        NoticeRemedy::ReclaimThenRecover {},
    ] {
        for frontend in [Frontend::Tui, Frontend::Run, Frontend::Jsonl] {
            let text = remedy_text(
                &remedy,
                RemedyContext {
                    frontend,
                    session_id: Some("current-id"),
                },
            );
            assert!(
                text.starts_with(&format!("the session store is {}; ", store.display())),
                "{text}"
            );
            let gc = text.find("`plak session gc --all`").unwrap();
            let delete = text.find("`plak session delete <id>`").unwrap();
            assert!(gc < delete, "{text}");
            assert!(
                text.ends_with(
                    "`plak state prune-legacy` frees only other builds' stores, not this one"
                ),
                "{text}"
            );
        }
    }
}
