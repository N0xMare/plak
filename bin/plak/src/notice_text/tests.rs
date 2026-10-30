use super::*;

fn all_remedies() -> Vec<NoticeRemedy> {
    use NoticeRemedy as R;
    vec![
        R::RecoverChildren,
        R::Rebind,
        R::Login {
            account: AccountTarget::Grok,
        },
        R::RetryThenLogin {
            account: AccountTarget::Grok,
        },
        R::StopProcesses,
        R::Attach,
        R::ResumeQueueOrStartFresh,
        R::ResumeSavedSession {
            session_id: "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap(),
        },
        R::RetryCompaction,
        R::AttachOnlyWhenDetached,
        R::ResumeTree,
        R::StartFresh,
        R::RetryOrChooseProfile,
        R::RecoverSession,
        R::InterruptWait,
        R::CancelContinuation,
        R::ResumeExactSession,
        R::ChooseProfile,
        R::ChooseProfileEffort,
        R::ChooseProfileOrAttach,
        R::RepairJavaScriptHost,
        R::RepairPythonHost,
        R::CarryCurrentLogin,
        R::RestoreAccountOrRebind {
            account: AccountTarget::Grok,
        },
        R::RestoreBindingOrCarry {
            account: AccountTarget::Grok,
        },
        R::LoginThenAttach {
            account: AccountTarget::Grok,
        },
        R::RepairThenAttach,
        R::RepairCredentialOrChooseProfile {
            account: AccountTarget::Grok,
            condition: CredentialCondition::NewerBuild,
        },
        R::CompactWholeGroups,
        R::CompactNative,
        R::CompactSegmentHistory,
        R::LargerModelOrStartFresh,
        R::RetryFork,
        R::ReclaimThenRecover,
        R::ReclaimRetainedHistory,
        R::FreeDiskSpace,
    ]
}

#[test]
fn every_remedy_has_each_existing_frontend_route_and_strict_encoding() {
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
                    NoticeRemedy::ResumeExactSession,
                    Frontend::Run | Frontend::Jsonl
                ) | (NoticeRemedy::AttachOnlyWhenDetached, Frontend::Run)
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
        render(&NoticeRemedy::ResumeTree, Frontend::Run)
            .contains("plak run --session current-id --resume-tree -- <prompt>")
    );
    assert!(render(&NoticeRemedy::ResumeTree, Frontend::Jsonl).contains("resume_tree"));
    assert!(
        render(&NoticeRemedy::ResumeQueueOrStartFresh, Frontend::Jsonl).contains("resume_queue")
    );
    assert!(render(&NoticeRemedy::ResumeQueueOrStartFresh, Frontend::Tui).contains("Tab then r"));
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
