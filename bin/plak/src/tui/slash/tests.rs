use super::*;

#[test]
fn harness_mode_is_not_a_slash_command() {
    assert!(!COMMANDS.iter().any(|(name, _)| *name == "plan"));
    assert!(matches!(
        resolve(
            &ParsedSlash {
                name: "plan".into(),
                args: String::new()
            },
            &SlashContext::default()
        ),
        SlashEffect::LocalNote(_)
    ));
}

#[test]
fn parse_table() {
    assert_eq!(
        parse_slash("/help"),
        Some(ParsedSlash {
            name: "help".into(),
            args: String::new()
        })
    );

    assert!(parse_slash("hello").is_none());
    assert_eq!(parse_slash("/CLEAR").unwrap().name, "clear");
}

#[test]
fn resolve_core_actions() {
    let ctx = SlashContext {
        phase: "idle".into(),
        ..SlashContext::default()
    };
    assert_eq!(
        resolve(&parse_slash("/clear").unwrap(), &ctx),
        SlashEffect::Action(UserAction::ClearSession)
    );
    assert_eq!(
        resolve(&parse_slash("/compact").unwrap(), &ctx),
        SlashEffect::Action(UserAction::Compact)
    );
    assert_eq!(
        resolve(&parse_slash("/config").unwrap(), &ctx),
        SlashEffect::Action(UserAction::Extension(SurfaceIntent::OpenConfigFile))
    );
    assert_eq!(
        resolve(&parse_slash("/resume").unwrap(), &ctx),
        SlashEffect::Action(UserAction::ResumeSession { id: None })
    );
    assert_eq!(
        resolve(&parse_slash("/tree").unwrap(), &ctx),
        SlashEffect::Action(UserAction::OpenTopologyNavigator)
    );
    assert_eq!(
        resolve(&parse_slash("/tree resume").unwrap(), &ctx),
        SlashEffect::Action(UserAction::ResumeTopologyTree)
    );
    assert_eq!(
        resolve(&parse_slash("/tree later").unwrap(), &ctx),
        SlashEffect::LocalNote("usage: /tree [resume]".to_owned())
    );
    assert_eq!(
        resolve(&parse_slash("/stop-all").unwrap(), &ctx),
        SlashEffect::Action(UserAction::StopAllTopology)
    );
    assert_eq!(
        resolve(&parse_slash("/resume deadbeef-uuid").unwrap(), &ctx),
        SlashEffect::Action(UserAction::ResumeSession {
            id: Some("deadbeef-uuid".into())
        })
    );
    assert_eq!(
        resolve(&parse_slash("/fork").unwrap(), &ctx),
        SlashEffect::Action(UserAction::ForkSession {
            account: None,
            compact: false,
        })
    );
    // The host refuses a fork onto another binding, so the catalog must not
    // advertise a target the resolver would only refuse.
    for line in ["/fork --to codex", "/fork codex"] {
        assert!(
            matches!(
                resolve(&parse_slash(line).unwrap(), &ctx),
                SlashEffect::LocalNote(note) if note.starts_with("usage: /fork")
            ),
            "{line} must not reach the host"
        );
    }
    let fork_help = COMMANDS
        .iter()
        .find(|(name, _)| *name == "fork")
        .map(|(_, help)| *help)
        .unwrap();
    assert!(!fork_help.contains("--to"), "{fork_help}");
    assert_eq!(
        resolve(&parse_slash("/usage").unwrap(), &ctx),
        SlashEffect::OpenUsage
    );
    assert_eq!(
        resolve(&parse_slash("/providers").unwrap(), &ctx),
        SlashEffect::OpenProviders
    );
    assert_eq!(
        resolve(&parse_slash("/context").unwrap(), &ctx),
        SlashEffect::OpenContext
    );
    assert_eq!(
        resolve(&parse_slash("/status").unwrap(), &ctx),
        SlashEffect::OpenSession
    );
    assert_eq!(
        resolve(&parse_slash("/settings").unwrap(), &ctx),
        SlashEffect::OpenSettings
    );
    assert_eq!(
        resolve(&parse_slash("/keybinds").unwrap(), &ctx),
        SlashEffect::OpenKeybinds
    );
    assert_eq!(
        resolve(&parse_slash("/quit").unwrap(), &ctx),
        SlashEffect::Quit
    );
}

#[test]
fn a_command_without_arguments_refuses_trailing_text() {
    let ctx = SlashContext::default();
    for (line, name) in [
        ("/compact now", "compact"),
        ("/clear all", "clear"),
        ("/reload please", "reload"),
        ("/rebind now", "rebind"),
        ("/quit later", "quit"),
        ("/exit now", "exit"),
        ("/drop 2", "drop"),
        ("/stop-all now", "stop-all"),
        ("/help me", "help"),
        ("/status  x", "status"),
        ("/CONFIG edit", "config"),
    ] {
        assert_eq!(
            resolve(&parse_slash(line).unwrap(), &ctx),
            SlashEffect::LocalNote(format!("usage: /{name}  —  takes no arguments")),
            "{line} must be refused, not run"
        );
    }
    // Every bare-word catalog row stays runnable without arguments.
    for name in TAKES_NO_ARGUMENTS {
        assert!(
            !matches!(
                resolve(&parse_slash(&format!("/{name}")).unwrap(), &ctx),
                SlashEffect::LocalNote(_)
            ),
            "/{name} must still run bare"
        );
    }
    // Commands with a grammar keep parsing their arguments.
    assert_eq!(
        resolve(&parse_slash("/resume abc").unwrap(), &ctx),
        SlashEffect::Action(UserAction::ResumeSession {
            id: Some("abc".into())
        })
    );

    // An unknown command is still named as unknown, arguments or not.
    assert!(matches!(
        resolve(&parse_slash("/nope now").unwrap(), &ctx),
        SlashEffect::LocalNote(note) if note.contains("unknown command /nope")
    ));
}

#[test]
fn resolve_local_notes() {
    let ctx = SlashContext {
        session_id: Some("s1".into()),
        account: Some("grok".into()),
        model: Some("m".into()),
        effort: Some("high".into()),
        phase: "idle".into(),
        queue_len: 2,
        busy: false,
    };
    match resolve(&parse_slash("/help").unwrap(), &ctx) {
        SlashEffect::OpenInfoOverlay { title, body } => {
            assert_eq!(title, "help");
            assert!(body.contains("/clear"));
        }
        other => panic!("expected help overlay, got {other:?}"),
    }
    // §2.1: `/status` routes to the Session tab, which builds every row
    // from typed host data (§5.5).
    assert_eq!(
        resolve(&parse_slash("/status").unwrap(), &ctx),
        SlashEffect::OpenSession
    );
    match resolve(&parse_slash("/keybinds").unwrap(), &ctx) {
        SlashEffect::OpenKeybinds => {}
        other => panic!("expected keybinds overlay, got {other:?}"),
    }
    match resolve(&parse_slash("/settings").unwrap(), &ctx) {
        SlashEffect::OpenSettings => {}
        other => panic!("expected the Settings tab, got {other:?}"),
    }
}

#[test]
fn the_catalog_is_closed_and_carries_no_deferred_capability_rows() {
    // Spec 02 §5.1: a deferred capability must not be advertised as a
    // command, and neither may a removed surface. Login, logout
    // and API keys are CLI-only; the roster is config-only.
    for dead in [
        "skills",
        "sandbox",
        "workflows",
        "login",
        "connect",
        "load-api-key",
    ] {
        assert!(
            !COMMANDS.iter().any(|(name, _)| *name == dead),
            "/{dead} must not be in the catalog"
        );
        assert!(fuzzy_list(dead).is_empty(), "/{dead} must not complete");
    }
    // Nothing left in the catalog is a placeholder row.
    for (name, help) in COMMANDS {
        assert!(
            !help.contains("not in v1"),
            "/{name} is still a placeholder row"
        );
        assert!(
            !help.contains("Control center"),
            "/{name} must not name the Menu tab: {help}"
        );
    }
}

#[test]
fn resolve_auth_and_binding() {
    let ctx = SlashContext {
        phase: "idle".into(),
        ..SlashContext::default()
    };
    assert_eq!(
        resolve(&parse_slash("/attach").unwrap(), &ctx),
        SlashEffect::Action(UserAction::RetryAttach)
    );
    for retired in ["/login", "/connect"] {
        assert!(
            matches!(
                resolve(&parse_slash(retired).unwrap(), &ctx),
                SlashEffect::LocalNote(note) if note.contains("unknown command")
            ),
            "{retired}"
        );
    }
    // The binding changes only through templates and profiles: the
    // retired binding commands are unknown commands, absent from the
    // palette.
    for retired in ["model", "account", "effort"] {
        assert!(!COMMANDS.iter().any(|(name, _)| *name == retired));
        assert!(fuzzy_list(retired).iter().all(|(name, _)| *name != retired));
        for line in [format!("/{retired}"), format!("/{retired} high")] {
            assert_eq!(
                resolve(&parse_slash(&line).unwrap(), &ctx),
                SlashEffect::LocalNote(format!("unknown command /{retired} — try /help")),
                "{line}"
            );
        }
    }
    // The product resolves the name, aliases included, from its list.
    assert_eq!(
        resolve(&parse_slash("/agents grok").unwrap(), &ctx),
        SlashEffect::ConfirmProfile("grok".into())
    );
    assert!(matches!(
        resolve(&parse_slash("/fork-then-compact").unwrap(), &ctx),
        SlashEffect::Action(UserAction::ForkSession { compact: true, .. })
    ));
}

#[test]
fn fuzzy_list_filters() {
    let all = fuzzy_list("");
    assert_eq!(all.len(), COMMANDS.len());
    let help = fuzzy_list("he");
    assert!(help.iter().any(|(n, _)| *n == "help"));
    let clr = fuzzy_list("clr"); // subsequence c-l-r in clear
    assert!(clr.iter().any(|(n, _)| *n == "clear"));
    assert_eq!(fuzzy_list("zzznotfound"), [] as [(&str, &str); 0]);
}
