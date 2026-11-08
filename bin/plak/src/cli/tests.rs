#![allow(
    clippy::too_many_lines,
    reason = "CLI grammar matrices keep accepted commands and exact located refusals beside their complete argv inputs."
)]

#[test]
fn host_stdio_bridge_requires_transport_and_refuses_server_flags() {
    assert!(matches!(
        parse(args(&["host", "connect", "--stdio"])).unwrap(),
        Command::HostConnect { auto_start: false }
    ));
    assert!(matches!(
        parse(args(&["host", "connect", "--stdio", "--auto-start"])).unwrap(),
        Command::HostConnect { auto_start: true }
    ));
    for arguments in [
        vec!["host", "connect"],
        vec!["host", "connect", "--stdio", "--web", "127.0.0.1:0"],
        vec!["host", "--web", "127.0.0.1:0", "connect", "--stdio"],
    ] {
        assert!(parse(args(&arguments)).is_err(), "{arguments:?}");
    }
}

#[test]
fn unknown_cli_accounts_share_admission_wording_and_registered_aliases() {
    for verb in ["run", "agent"] {
        let mut arguments = vec![verb, "--account", "removed/work", "--model", "m"];
        if verb == "run" {
            arguments.extend(["--", "hello"]);
        }
        let error = parse(args(&arguments)).unwrap_err();
        assert!(error.contains("unknown_account: `removed/work`"), "{error}");
    }
    assert!(matches!(
        parse(args(&["login", "chatgpt"])).unwrap(),
        Command::AuthLogin {
            provider,
            ..
        } if provider.as_str() == "codex"));
    assert!(matches!(
        parse(args(&["refresh", "xai-oauth"])).unwrap(),
        Command::AuthRefresh(account) if account.as_str() == "grok"
    ));
}

#[test]
fn named_cli_accounts_keep_the_full_reference_on_every_command_surface() {
    for account in ["claude/work", "codex/work", "grok/work"] {
        for verb in ["status", "refresh", "models"] {
            let parsed = parse(args(&[verb, account])).unwrap();
            let selected = match parsed {
                Command::AuthStatus(selected)
                | Command::AuthRefresh(selected)
                | Command::Models(selected) => selected,
                other => panic!("unexpected command: {other:?}"),
            };
            assert_eq!(selected.as_str(), account);
        }
        assert!(
            matches!(parse(args(&["login", account])).unwrap(), Command::AuthLogin { provider, .. } if provider.as_str() == account)
        );
        assert!(
            matches!(parse(args(&["logout", account, "--local"])).unwrap(), Command::AuthLogout { provider, local_only: true } if provider.as_str() == account)
        );
        assert!(
            matches!(parse(args(&["run", "--account", account, "--model", "m", "--", "hello"])).unwrap(), Command::Run { provider, .. } if provider.as_str() == account)
        );
        assert!(
            matches!(parse(args(&["agent", "--account", account, "--model", "m"])).unwrap(), Command::Agent { account: Some(selected), .. } if selected.as_str() == account)
        );
    }
    for account in [
        "openai/work",
        "anthropic/work",
        "xai-api/work",
        "openrouter/work",
    ] {
        assert!(
            matches!(parse(args(&["key", "set", account])).unwrap(), Command::KeySet { account: selected } if selected.as_str() == account)
        );
        assert!(
            matches!(parse(args(&["key", "clear", account])).unwrap(), Command::KeyClear { account: selected } if selected.as_str() == account)
        );
        assert!(
            matches!(parse(args(&["status", account])).unwrap(), Command::KeyStatus { account: Some(selected) } if selected.as_str() == account)
        );
        assert!(parse(args(&["login", account])).is_err());
        assert!(parse(args(&["refresh", account])).is_err());
    }
    assert!(matches!(
        parse(args(&["accounts", "list"])).unwrap(),
        Command::AccountsList
    ));
    assert!(
        matches!(parse(args(&["accounts", "rm", "claude/work", "--yes"])).unwrap(), Command::AccountsRemove { account, confirmed: true } if account.as_str() == "claude/work")
    );
    assert!(parse(args(&["accounts", "rm", "claude/work", "--local"])).is_err());
}

#[test]
fn run_image_flags_attach_sniffed_files_and_name_the_refused_file() {
    let dir = tempfile::tempdir().unwrap();
    let shot = dir.path().join("shot.png");
    std::fs::write(&shot, plakous_hosted::attachments::png(4, 3, 10)).unwrap();
    let notes = dir.path().join("notes.png");
    std::fs::write(&notes, "plain text").unwrap();
    let run = |images: &[&std::path::Path], account: &str| {
        let mut arguments = vec![
            OsString::from("run"),
            "--account".into(),
            account.into(),
            "--model".into(),
            "m".into(),
        ];
        for image in images {
            arguments.push("--image".into());
            arguments.push(image.as_os_str().to_owned());
        }
        arguments.extend(["--".into(), "what is this?".into()]);
        parse(arguments)
    };
    let command = run(&[&shot, &shot], "grok").unwrap();
    let Command::Run { prompt, .. } = command else {
        panic!("expected run")
    };
    assert_eq!(prompt.text, "what is this?");
    assert_eq!(prompt.images.len(), 2);
    assert_eq!(prompt.images[0].media_type, "image/png");
    assert_eq!(prompt.images[0].name, "shot.png");
    let Command::RunOpenRouter { prompt, .. } = run(&[&shot], "openrouter").unwrap() else {
        panic!("expected openrouter run")
    };
    assert_eq!(prompt.images.len(), 1);

    // Images alone are a prompt.
    let image_only = parse(vec![
        OsString::from("run"),
        "--account".into(),
        "grok".into(),
        "--model".into(),
        "m".into(),
        "--image".into(),
        shot.as_os_str().to_owned(),
    ]);
    let Ok(Command::Run { prompt, .. }) = image_only else {
        panic!("expected an image-only run, got {image_only:?}")
    };
    assert_eq!(prompt.text, "");
    assert_eq!(prompt.images.len(), 1);

    let error = run(&[&notes], "grok").unwrap_err();
    assert!(
        error.contains("notes.png") && error.contains("not a supported image"),
        "{error}"
    );
    let missing = dir.path().join("missing.png");
    let error = run(&[&missing], "grok").unwrap_err();
    assert!(
        error.contains("missing.png") && error.contains("cannot be read"),
        "{error}"
    );
    let nine = vec![shot.as_path(); 9];
    let error = run(&nine, "grok").unwrap_err();
    assert!(error.contains("at most 8"), "{error}");

    let largest = dir.path().join("largest.png");
    let header = plakous_hosted::attachments::png(4, 3, 0).len();
    std::fs::write(
        &largest,
        plakous_hosted::attachments::png(
            4,
            3,
            plakous_hosted::attachments::MAX_IMAGE_BYTES - header,
        ),
    )
    .unwrap();
    let Ok(Command::Run { prompt, .. }) = run(&[largest.as_path(); 3], "grok") else {
        panic!("three maximum-size images fit one message")
    };
    assert_eq!(prompt.images.len(), 3);
    let error = run(&[largest.as_path(); 4], "grok").unwrap_err();
    assert!(
        error.contains("over the 16 MiB per-message budget"),
        "{error}"
    );
}

#[test]
fn publication_help_after_separator_is_prompt_text() {
    let command = parse(
        [
            "run",
            "--account",
            "grok",
            "--model",
            "test",
            "--",
            "--help",
        ]
        .map(OsString::from),
    )
    .unwrap();
    assert!(matches!(command, Command::Run { prompt, .. } if prompt == "--help"));
}

#[test]
fn publication_api_invalid_effort_names_ladder() {
    let error = parse(
        [
            "run",
            "--account",
            "openai",
            "--model",
            "gpt-5",
            "--effort",
            "bogus",
            "--",
            "task",
        ]
        .map(OsString::from),
    )
    .unwrap_err();
    assert_eq!(error, crate::command::run_effort_usage());
}

#[test]
fn publication_parse_errors_name_verb_without_echoing_arguments() {
    let error = parse(["run", "secret-prompt"].map(OsString::from)).unwrap_err();
    assert!(error.contains("put the prompt after `--`"));
    assert!(!error.contains("secret-prompt"));
    let error = parse(["session", "--unknown"].map(OsString::from)).unwrap_err();
    assert!(error.contains("plak session --help"));
    assert!(!error.contains("<command>"));
}

#[test]
fn publication_every_visible_clap_flag_is_on_verb_help_page() {
    use clap::CommandFactory as _;
    fn check(command: &clap::Command, page: &str) {
        for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
            if let Some(long) = arg.get_long() {
                assert!(
                    page.contains(&format!("--{long}")),
                    "{} missing --{long}",
                    command.get_name()
                );
            }
        }
        for subcommand in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
            check(subcommand, page);
        }
    }
    for command in Cli::command()
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
    {
        let mut page = Vec::new();
        render::command_help(command.get_name(), &mut page).unwrap();
        check(command, &String::from_utf8(page).unwrap());
    }
}

#[test]
fn run_and_agent_help_list_python_sdk_composition() {
    for verb in ["run", "agent"] {
        let mut page = Vec::new();
        render::command_help(verb, &mut page).unwrap();
        let page = String::from_utf8(page).unwrap();
        let composition = page
            .lines()
            .find(|line| line.contains("--composition <c>"))
            .unwrap();
        assert!(composition.contains("py-sdk"), "{verb}: {composition}");
    }
}

#[test]
fn run_accepts_turn_and_wall_clock_bounds() {
    let args = [
        "run",
        "--account",
        "grok",
        "--model",
        "test",
        "--max-turns",
        "2",
        "--timeout",
        "3",
        "--",
        "task",
    ]
    .map(str::to_owned);
    assert!(parse(args.into_iter().map(OsString::from)).is_ok());
    for bound in ["0", "101"] {
        assert!(
            parse(
                [
                    "run",
                    "--account",
                    "grok",
                    "--model",
                    "test",
                    "--max-turns",
                    bound,
                    "--",
                    "task"
                ]
                .into_iter()
                .map(OsString::from)
            )
            .is_err()
        );
    }
}
use super::*;
use crate::command::Command;
use crate::home::paths::SecretShelf;
use proptest::prelude::*;

fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    super::parse(arguments).map_err(|failure| failure.message().to_owned())
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(|s| OsString::from(*s)).collect()
}

#[test]
fn bare_plak_opens_tui() {
    assert!(matches!(
        parse(Vec::<OsString>::new()),
        Ok(Command::Tui { .. })
    ));
}

#[test]
fn help_and_login_parse() {
    assert!(matches!(parse(["help".into()]), Ok(Command::Help)));
    assert!(matches!(
        parse(["login".into(), "grok".into()]),
        Ok(Command::AuthLogin {
            provider,
            ..
        }) if provider.as_str() == "grok"));
    assert!(matches!(
        parse(["login".into(), "claude".into()]),
        Ok(Command::AuthLogin {
            provider,
            ..
        }) if provider.as_str() == "claude"));
    assert!(matches!(
        parse(["login".into(), "--help".into()]),
        Ok(Command::CommandHelp { command }) if command == "login"
    ));
}

#[test]
fn clap_routes_status() {
    assert!(matches!(
        parse(["status".into(), "codex".into()]),
        Ok(Command::AuthStatus(account)) if account.as_str() == "codex"
    ));
}

#[test]
fn run_session_rejects_conflicting_binding_flags() {
    let err = parse([
        "run".into(),
        "--session".into(),
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
        "--account".into(),
        "codex".into(),
        "--".into(),
        "hello".into(),
    ]);
    let Err(message) = err else {
        panic!("session plus account must fail closed");
    };
    assert!(message.contains("recorded binding"));
}

#[test]
fn run_and_agent_refuse_the_removed_resume_tree_flag() {
    for args in [
        vec![
            "run",
            "--session",
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "--resume-tree",
            "--",
            "continue",
        ],
        vec![
            "agent",
            "--session",
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "--resume-tree",
        ],
    ] {
        let Err(message) = parse(args.into_iter().map(OsString::from)) else {
            panic!("--resume-tree is not a flag");
        };
        assert!(message.starts_with("unknown option"), "{message}");
    }
}

#[test]
fn agent_stdio_is_the_default_pipe() {
    let cmd = parse(["agent".into()]).unwrap();
    match cmd {
        Command::Agent {
            session_id: None, ..
        } => {}
        _ => panic!("expected bare agent"),
    }
}

#[test]
fn openrouter_agent_resolves_auto_once_and_defaults_follow_catalog() {
    let command = parse(args(&[
        "agent",
        "--account",
        "openrouter",
        "--model",
        "vendor/model:free",
    ]))
    .unwrap();
    assert!(matches!(
        command,
        Command::Agent {
            provider: None,
            platform: None,
            wire: Some(crate::product_config::ProductWireBinding {
                selection: crate::product_config::ProductProtocolSelection::Auto,
                resolved: crate::product_config::ProductResolvedProtocol::Chat,
                ..
            }),
            model: Some(model),
            effort: None,
            effort_explicit: false,
            composition:
                Some(crate::product_config::ProductCompositionSelection::FollowCatalog),
            ..
        } if model == "vendor/model:free"
    ));
}

#[test]
fn openrouter_agent_protocol_and_composition_admission_is_closed_pre_io() {
    for (protocol, resolved) in [
        (
            "responses",
            crate::product_config::ProductResolvedProtocol::Responses,
        ),
        (
            "messages",
            crate::product_config::ProductResolvedProtocol::Messages,
        ),
    ] {
        let command = parse(args(&[
            "agent",
            "--account",
            "openrouter",
            "--model",
            "vendor/model",
            "--protocol",
            protocol,
        ]))
        .unwrap();
        assert!(matches!(
            command,
            Command::Agent {
                wire: Some(wire),
                ..
            } if wire.resolved == resolved
        ));
    }
    let native_ptc = parse(args(&[
        "agent",
        "--account",
        "openrouter",
        "--model",
        "vendor/model",
        "--composition",
        "native-ptc",
    ]))
    .unwrap_err();
    assert!(
        native_ptc.contains("OpenRouter has no native programmatic tool calling")
            && native_ptc.contains("use js-ptc, py-ptc or direct"),
        "{native_ptc}"
    );
    let legacy_native = parse(args(&[
        "agent",
        "--account",
        "openrouter",
        "--model",
        "vendor/model",
        "--composition",
        "native",
    ]))
    .unwrap_err();
    assert!(legacy_native.contains("direct|plakous-default|native-ptc|js-ptc|py-ptc"));
    let oauth_protocol = parse(args(&[
        "agent",
        "--account",
        "codex",
        "--model",
        "gpt-5.3-codex-spark",
        "--protocol",
        "chat",
    ]))
    .unwrap_err();
    assert!(oauth_protocol.contains("only with --account openrouter"));
}

#[test]
fn openrouter_agent_account_parses_without_oauth_provider() {
    let command = parse(args(&[
        "agent",
        "--account",
        "openrouter",
        "--model",
        "vendor/model",
    ]))
    .unwrap();
    assert!(matches!(
        command,
        Command::Agent {
            provider: None,
            platform: None,
            wire: Some(_),
            ..
        }
    ));
}

#[test]
fn clap_routes_run_with_delimiter() {
    let cmd = parse([
        "run".into(),
        "--account".into(),
        "codex".into(),
        "--model".into(),
        "gpt-5.3-codex-spark".into(),
        "--".into(),
        "hello".into(),
    ])
    .unwrap();
    match cmd {
        Command::Run {
            provider,
            model,
            prompt,
            ..
        } => {
            assert_eq!(provider.as_str(), "codex");
            assert_eq!(model, "gpt-5.3-codex-spark");
            assert_eq!(prompt, "hello");
        }
        _ => panic!("expected run"),
    }
}

#[test]
fn openrouter_run_resolves_auto_once_and_defaults_follow_catalog() {
    let command = parse(args(&[
        "run",
        "--account",
        "openrouter",
        "--model",
        "vendor/model:free",
        "--",
        "hello",
    ]))
    .unwrap();
    assert!(matches!(
        command,
        Command::RunOpenRouter {
            model,
            effort: None,
            composition: crate::product_config::ProductCompositionSelection::FollowCatalog,
            wire: crate::product_config::ProductWireBinding {
                selection: crate::product_config::ProductProtocolSelection::Auto,
                resolved: crate::product_config::ProductResolvedProtocol::Chat,
                ..
            },
            prompt,
            ..
        } if model == "vendor/model:free" && prompt == "hello"
    ));
}

#[test]
fn openrouter_protocol_and_composition_admission_is_closed_pre_io() {
    for (protocol, resolved) in [
        (
            "responses",
            crate::product_config::ProductResolvedProtocol::Responses,
        ),
        (
            "messages",
            crate::product_config::ProductResolvedProtocol::Messages,
        ),
    ] {
        let command = parse(args(&[
            "run",
            "--account",
            "openrouter",
            "--model",
            "vendor/model",
            "--protocol",
            protocol,
            "--",
            "hello",
        ]))
        .unwrap();
        assert!(matches!(
            command,
            Command::RunOpenRouter { wire, .. } if wire.resolved == resolved
        ));
    }
    let native_ptc = parse(args(&[
        "run",
        "--account",
        "openrouter",
        "--model",
        "vendor/model",
        "--composition",
        "native-ptc",
        "--",
        "hello",
    ]))
    .unwrap_err();
    assert!(
        native_ptc.contains("OpenRouter has no native programmatic tool calling")
            && native_ptc.contains("use js-ptc, py-ptc or direct"),
        "{native_ptc}"
    );
    let legacy_native = parse(args(&[
        "run",
        "--account",
        "openrouter",
        "--model",
        "vendor/model",
        "--composition",
        "native",
        "--",
        "hello",
    ]))
    .unwrap_err();
    assert!(legacy_native.contains("direct|plakous-default|native-ptc|js-ptc|py-ptc"));
    let oauth_protocol = parse(args(&[
        "run",
        "--account",
        "codex",
        "--model",
        "gpt-5.3-codex-spark",
        "--protocol",
        "chat",
        "--",
        "hello",
    ]))
    .unwrap_err();
    assert!(oauth_protocol.contains("only with --account openrouter"));
}

#[test]
fn run_composition_selector_accepts_closed_cli_set() {
    for (value, expected) in [
        (
            "direct",
            crate::product_config::ProductCompositionSelection::Direct,
        ),
        (
            "plakous-default",
            crate::product_config::ProductCompositionSelection::FollowCatalog,
        ),
        (
            "native-ptc",
            crate::product_config::ProductCompositionSelection::NativePtc,
        ),
        (
            "js-ptc",
            crate::product_config::ProductCompositionSelection::JsPtc,
        ),
        (
            "py-ptc",
            crate::product_config::ProductCompositionSelection::PyPtc,
        ),
        (
            "py-sdk",
            crate::product_config::ProductCompositionSelection::PySdk,
        ),
    ] {
        let command = parse(args(&[
            "run",
            "--account",
            "grok",
            "--model",
            "grok-4.6",
            "--effort",
            "low",
            "--composition",
            value,
            "--",
            "hello",
        ]))
        .unwrap();
        assert!(matches!(
            command,
            Command::Run {
                composition: Some(actual),
                ..
            } if actual == expected
        ));
    }
    assert!(
        parse(args(&[
            "run",
            "--account",
            "grok",
            "--model",
            "grok-4.6",
            "--composition",
            "py",
            "--",
            "hello",
        ]))
        .unwrap_err()
        .contains("direct|plakous-default|native-ptc|js-ptc|py-ptc")
    );
    assert!(
        parse(args(&[
            "run",
            "--account",
            "grok",
            "--model",
            "grok-4.6",
            "--composition",
            "javascript,python",
            "--",
            "hello",
        ]))
        .unwrap_err()
        .contains("direct|plakous-default|native-ptc|js-ptc|py-ptc")
    );
}

#[test]
fn clap_routes_claude_oauth_run_sonnet_5_low() {
    let cmd = parse([
        "run".into(),
        "--account".into(),
        "claude".into(),
        "--model".into(),
        "claude-sonnet-5".into(),
        "--effort".into(),
        "low".into(),
        "--".into(),
        "pong".into(),
    ])
    .unwrap();
    match cmd {
        Command::Run {
            provider,
            model,
            effort,
            prompt,
            ..
        } => {
            assert_eq!(provider.as_str(), "claude");
            assert_eq!(model, "claude-sonnet-5");
            assert_eq!(effort, Some(plakous::model::ReasoningEffort::Low));
            assert_eq!(prompt, "pong");
        }
        _ => panic!("expected claude run"),
    }
}

#[test]
fn prune_legacy_missing_launches_requires_explicit_flag() {
    assert!(
        parse(args(&[
            "state",
            "prune-legacy",
            "--yes",
            "--forget-missing-launches"
        ]))
        .is_ok()
    );
}

/// P0: product argv matrix — each public command parses (or documented reject).
#[test]
#[allow(
    clippy::type_complexity,
    reason = "table of argv slices → predicate is clearer than a macro here"
)]
fn product_command_argv_matrix() {
    let cases: &[(&[&str], fn(&Command) -> bool)] = &[
        (&["help"], |c| matches!(c, Command::Help)),
        (&["version"], |c| matches!(c, Command::Version)),
        (&["-V"], |c| matches!(c, Command::Version)),
        (&["--version"], |c| matches!(c, Command::Version)),
        (&["-h"], |c| matches!(c, Command::Help)),
        (&["doctor"], |c| matches!(c, Command::Doctor)),
        (&["state", "prune-legacy"], |c| {
            matches!(
                c,
                Command::StatePruneLegacy {
                    yes: false,
                    forget_missing_launches: false,
                    prune_orphaned_references: false
                }
            )
        }),
        (&["state", "prune-legacy", "--yes"], |c| {
            matches!(
                c,
                Command::StatePruneLegacy {
                    yes: true,
                    forget_missing_launches: false,
                    prune_orphaned_references: false
                }
            )
        }),
        (&["config", "show"], |c| matches!(c, Command::ConfigShow)),
        (&["config", "init"], |c| matches!(c, Command::ConfigInit)),
        (
            &["config", "set", "account", "grok"],
            |c| matches!(c, Command::ConfigSet { key, value, scope } if key == "account" && value == "grok" && *scope == crate::product_config::ConfigWriteScope::User),
        ),
        (
            &["config", "set", "tui.theme", "light", "--scope", "project"],
            |c| matches!(c, Command::ConfigSet { scope, .. } if *scope == crate::product_config::ConfigWriteScope::Project),
        ),
        (&["config", "trust"], |c| matches!(c, Command::ConfigTrust)),
        (&["config", "untrust"], |c| {
            matches!(c, Command::ConfigUntrust)
        }),
        (&["session", "list"], |c| {
            matches!(
                c,
                Command::SessionList {
                    include_archived: false
                }
            )
        }),
        (
            &["session", "show", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"],
            |c| matches!(c, Command::SessionShow { id } if id.starts_with("aaaa")),
        ),
        (
            &["session", "log", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"],
            |c| matches!(c, Command::SessionLog { id, .. } if id.starts_with("aaaa")),
        ),
        (
            &["session", "export", "aaaa", "--out", "/tmp/x.json"],
            |c| matches!(c, Command::SessionExport { id, out: Some(out) } if id == "aaaa" && out.as_os_str() == "/tmp/x.json"),
        ),
        (&["session", "export", "aaaa"], |c| {
            matches!(c, Command::SessionExport { out: None, .. })
        }),
        (
            &["session", "import", "/tmp/x.json"],
            |c| matches!(c, Command::SessionImport { path } if path.as_os_str() == "/tmp/x.json"),
        ),
        (&["session", "export"], |c| {
            matches!(c, Command::CommandHelp { .. })
        }),
        (
            &["session", "delete", "aaaa"],
            |c| matches!(c, Command::SessionDelete { id, yes: false, abandon_children: false } if id == "aaaa"),
        ),
        (&["session", "delete", "aaaa", "--yes"], |c| {
            matches!(c, Command::SessionDelete { yes: true, .. })
        }),
        (&["session", "delete"], |c| {
            matches!(c, Command::CommandHelp { .. })
        }),
        (
            &["session", "gc", "aaaa"],
            |c| matches!(c, Command::SessionGc { id: Some(id) } if id == "aaaa"),
        ),
        (&["session", "gc", "--all"], |c| {
            matches!(c, Command::SessionGc { id: None })
        }),
        (&["session", "gc"], |c| {
            matches!(c, Command::CommandHelp { .. })
        }),
        (
            &["version", "--write-binary-manifest", "/tmp/snapshot"],
            |c| matches!(c, Command::WriteBinaryManifest { directory } if directory.as_os_str() == "/tmp/snapshot"),
        ),
        (&["version", "--state-key"], |c| {
            matches!(c, Command::StateKey)
        }),
        (
            &["--import-resume", "/tmp/x.json"],
            |c| matches!(c, Command::ImportResume { path, handoff: None } if path.as_os_str() == "/tmp/x.json"),
        ),
        (
            &[
                "--import-resume",
                "/tmp/x.json",
                "--reload-handoff",
                "/tmp/h.json",
            ],
            |c| matches!(c, Command::ImportResume { path, handoff: Some(handoff) } if path.as_os_str() == "/tmp/x.json" && handoff.as_os_str() == "/tmp/h.json"),
        ),
        (
            &["--resume", "aaaaaaaa", "--reload-handoff", "/tmp/h.json"],
            |c| matches!(c, Command::Resume { session_id: Some(id), handoff: Some(handoff) } if id == "aaaaaaaa" && handoff.as_os_str() == "/tmp/h.json"),
        ),
        (&["status"], |c| matches!(c, Command::StatusAll)),
        (
            &["models", "claude"],
            |c| matches!(c, Command::Models(account) if account.as_str() == "claude"),
        ),
        (
            &["models", "codex"],
            |c| matches!(c, Command::Models(account) if account.as_str() == "codex"),
        ),
        (
            &["status", "codex"],
            |c| matches!(c, Command::AuthStatus(account) if account.as_str() == "codex"),
        ),
        (&["login", "codex"], |c| {
            matches!(
                c,
                Command::AuthLogin {
                    provider,
                    ..
                } if provider.as_str() == "codex")
        }),
        (&["login", "claude"], |c| {
            matches!(
                c,
                Command::AuthLogin {
                    provider,
                    mode: NativeLoginMode::Browser,
                } if provider.as_str() == "claude")
        }),
        (&["login", "claude", "--remote"], |c| {
            matches!(
                c,
                Command::AuthLogin {
                    provider,
                    mode: NativeLoginMode::Remote,
                } if provider.as_str() == "claude")
        }),
        (&["logout", "grok", "--local"], |c| {
            matches!(
                c,
                Command::AuthLogout {
                    provider,
                    local_only: true
                } if provider.as_str() == "grok")
        }),
        (&["logout", "supergrok", "--local"], |c| {
            matches!(
                c,
                Command::AuthLogout {
                    provider,
                    local_only: true
                } if provider.as_str() == "grok")
        }),
        (
            &["refresh", "grok"],
            |c| matches!(c, Command::AuthRefresh(account) if account.as_str() == "grok"),
        ),
        (&["key", "status"], |c| {
            matches!(c, Command::KeyStatus { account: None })
        }),
        (
            &["key", "status", "openai"],
            |c| matches!(c, Command::KeyStatus { account: Some(a) } if a.as_str() == "openai"),
        ),
        (
            &["key", "clear", "openai"],
            |c| matches!(c, Command::KeyClear { account } if account.as_str() == "openai"),
        ),
        (
            &["key", "set", "xai-api"],
            |c| matches!(c, Command::KeySet { account } if account.as_str() == "xai-api"),
        ),
        (
            &["secret", "set", "github"],
            |c| matches!(c, Command::SecretSet { shelf: SecretShelf::Named, name } if name == "github"),
        ),
        (
            &["secret", "rm", "linear-ws"],
            |c| matches!(c, Command::SecretRm { shelf: SecretShelf::Named, name } if name == "linear-ws"),
        ),
        (&["secret", "list"], |c| {
            matches!(
                c,
                Command::SecretList {
                    shelf: SecretShelf::Named
                }
            )
        }),
        (
            &["runner", "secret", "set", "github"],
            |c| matches!(c, Command::SecretSet { shelf: SecretShelf::Runner, name } if name == "github"),
        ),
        (
            &["runner", "secret", "rm", "github"],
            |c| matches!(c, Command::SecretRm { shelf: SecretShelf::Runner, name } if name == "github"),
        ),
        (&["runner", "secret", "list"], |c| {
            matches!(
                c,
                Command::SecretList {
                    shelf: SecretShelf::Runner
                }
            )
        }),
        (
            &["runner", "secret", "set"],
            |c| matches!(c, Command::CommandHelp { command } if command == "runner"),
        ),
        (&["runner", "serve"], |c| {
            matches!(
                c,
                Command::Runner(crate::commands::runner::RunnerCommand::Serve)
            )
        }),
        (&["runner", "serve", "--local"], |c| {
            matches!(
                c,
                Command::Runner(crate::commands::runner::RunnerCommand::ServeLocal)
            )
        }),
        (&["runner", "connect", "--stdio"], |c| {
            matches!(
                c,
                Command::Runner(crate::commands::runner::RunnerCommand::Connect)
            )
        }),
        (
            &["runner", "connect"],
            |c| matches!(c, Command::CommandHelp { command } if command == "runner"),
        ),
        (&["runner", "ps"], |c| {
            matches!(
                c,
                Command::Runner(crate::commands::runner::RunnerCommand::Ps { name: None })
            )
        }),
        (&["runner", "pair", "build"], |c| {
            matches!(
                c,
                Command::Runner(crate::commands::runner::RunnerCommand::Pair { name })
                    if name == "build"
            )
        }),
        (
            &["runner"],
            |c| matches!(c, Command::CommandHelp { command } if command == "runner"),
        ),
        (
            &["secret"],
            |c| matches!(c, Command::CommandHelp { command } if command == "secret"),
        ),
        (
            &["secret", "set"],
            |c| matches!(c, Command::CommandHelp { command } if command == "secret"),
        ),
        (&["resume"], |c| {
            matches!(
                c,
                Command::Resume {
                    session_id: None,
                    handoff: None
                }
            )
        }),
        (
            &["resume", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"],
            |c| matches!(c, Command::Resume { session_id: Some(id), handoff: None } if id.starts_with("aaaa")),
        ),
        (&["--resume"], |c| {
            matches!(
                c,
                Command::Resume {
                    session_id: None,
                    handoff: None
                }
            )
        }),
        (
            &["usage", "--session", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"],
            |c| {
                matches!(
                    c,
                    Command::Usage {
                        session_id: Some(_),
                        ..
                    }
                )
            },
        ),
        (&["usage"], |c| {
            matches!(
                c,
                Command::Usage {
                    account: None,
                    session_id: None
                }
            )
        }),
        (
            &["run", "--account", "grok", "--model", "m", "--", "hi"],
            |c| matches!(c, Command::Run { provider, prompt, .. } if provider.as_str() == "grok" && prompt == "hi"),
        ),
        (
            &[
                "run",
                "--session",
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                "--",
                "continue",
            ],
            |c| {
                matches!(
                    c,
                    Command::ContinueSession { session_id, prompt, .. }
                        if session_id.starts_with("aaaa") && prompt == "continue"
                )
            },
        ),
        // Recursive help
        (
            &["config", "--help"],
            |c| matches!(c, Command::CommandHelp { command } if command == "config"),
        ),
        (
            &["help", "run"],
            |c| matches!(c, Command::CommandHelp { command } if command == "run"),
        ),
    ];

    for (argv, check) in cases {
        let parsed = parse(args(argv));
        assert!(parsed.is_ok(), "expected Ok for {argv:?}, got {parsed:?}");
        let cmd = parsed.unwrap();
        assert!(check(&cmd), "predicate failed for {argv:?} → {cmd:?}");
    }
}

/// `telemetry prune` mutates the store, so a bare invocation refuses.
#[test]
fn telemetry_prune_requires_a_bound_or_an_explicit_yes() {
    assert!(
        parse(args(&["telemetry", "prune"]))
            .unwrap_err()
            .contains("--yes")
    );
    assert!(matches!(
        parse(args(&["telemetry", "prune", "--yes"])),
        Ok(Command::TelemetryPrune {
            max_age_days: None,
            max_bytes: None
        })
    ));
    assert!(matches!(
        parse(args(&["telemetry", "prune", "--max-age-days", "7"])),
        Ok(Command::TelemetryPrune {
            max_age_days: Some(7),
            ..
        })
    ));
}

/// `plak config path` is retired: `config show` prints `config_path=` first.
#[test]
fn config_path_is_retired_in_favour_of_config_show() {
    assert!(parse(args(&["config", "path"])).is_err());
    assert!(matches!(
        parse(args(&["config", "show"])),
        Ok(Command::ConfigShow)
    ));
}

#[test]
fn secret_values_never_come_from_argv_and_names_follow_the_grammar() {
    for argv in [
        &["secret", "set", "github", "ghp-value"][..],
        &["secret", "set", "Bad"][..],
        &["secret", "rm", "../x"][..],
        &["secret", "list", "github"][..],
    ] {
        assert!(parse(args(argv)).is_err(), "must reject {argv:?}");
    }
}

#[test]
fn mcp_add_takes_secret_names_never_values_and_one_transport() {
    use crate::commands::mcp::McpCommand;
    use crate::product_config::McpTransport;
    let Ok(Command::Mcp(McpCommand::Add { id, server, .. })) = parse(args(&[
        "mcp",
        "add",
        "github",
        "--secret-env",
        "GITHUB_PERSONAL_ACCESS_TOKEN=github",
        "--tools",
        "get_me,search_code",
        "--",
        "docker",
        "run",
        "-i",
        "--rm",
        "img",
    ])) else {
        panic!("stdio add must parse");
    };
    assert_eq!(id, "github");
    assert_eq!(
        server.tools.as_deref(),
        Some(&["get_me".to_owned(), "search_code".to_owned()][..])
    );
    let McpTransport::Stdio {
        command,
        args: argv,
        secret_env,
        ..
    } = server.transport
    else {
        panic!("expected stdio");
    };
    assert_eq!(command, "docker");
    assert_eq!(argv, ["run", "-i", "--rm", "img"]);
    assert_eq!(secret_env["GITHUB_PERSONAL_ACCESS_TOKEN"], "github");
    let Ok(Command::Mcp(McpCommand::Add { server, .. })) = parse(args(&[
        "mcp",
        "add",
        "docs",
        "--url",
        "https://docs.test/mcp",
        "--oauth",
        "--oauth-issuer",
        "https://auth.test",
        "--oauth-client-id",
        "plak",
        "--oauth-client-secret",
        "docs-client",
        "--oauth-callback-port",
        "4321",
        "--resources",
    ])) else {
        panic!("oauth add must parse");
    };
    assert!(server.resources);
    let McpTransport::Http { oauth, .. } = server.transport else {
        panic!("expected http");
    };
    assert_eq!(
        oauth,
        Some(crate::product_config::McpOAuthConfig {
            issuer: Some("https://auth.test".to_owned()),
            client: Some(crate::product_config::McpOAuthClient {
                id: "plak".to_owned(),
                secret: Some("docs-client".to_owned()),
            }),
            callback_port: Some(4321),
        })
    );
    assert!(matches!(
        parse(args(&[
            "mcp",
            "add",
            "gh",
            "--url",
            "https://x.test/mcp",
            "--bearer-secret",
            "github"
        ])),
        Ok(Command::Mcp(McpCommand::Add { .. }))
    ));
    for argv in [
        &["mcp", "add", "gh"][..],
        &["mcp", "add", "gh", "--url", "https://x.test", "--", "cmd"][..],
        &[
            "mcp",
            "add",
            "gh",
            "--url",
            "https://x.test",
            "--env",
            "A=b",
        ][..],
        &["mcp", "add", "gh", "--bearer-secret", "s", "--", "cmd"][..],
        &["mcp", "add", "gh", "--secret-env", "NOEQUALS", "--", "cmd"][..],
        &["mcp", "enable", "gh"][..],
        &["mcp", "add", "gh", "--oauth", "--", "cmd"][..],
        &[
            "mcp",
            "add",
            "gh",
            "--url",
            "https://x.test",
            "--oauth-issuer",
            "https://auth.test",
        ][..],
    ] {
        assert!(parse(args(argv)).is_err(), "must reject {argv:?}");
    }
    for argv in [
        &[
            "mcp",
            "add",
            "gh",
            "--url",
            "https://x.test",
            "--bearer-secret",
            "ghp_Pasted_Value",
        ][..],
        &[
            "mcp",
            "add",
            "gh",
            "--secret-env",
            "T=ghp_Pasted_Value",
            "--",
            "cmd",
        ][..],
        &[
            "mcp",
            "add",
            "gh",
            "--url",
            "https://x.test",
            "--header-secret",
            "X-Key=ghp_Pasted_Value",
        ][..],
        &[
            "mcp",
            "add",
            "gh",
            "--url",
            "https://x.test",
            "--header",
            "ghp_Pasted_Value",
        ][..],
        &[
            "mcp",
            "add",
            "gh",
            "--url",
            "https://x.test",
            "--oauth",
            "--oauth-issuer",
            "https://auth.test",
            "--oauth-client-id",
            "plak",
            "--oauth-client-secret",
            "ghp_Pasted_Value",
        ][..],
    ] {
        let refused = parse(args(argv)).unwrap_err();
        assert!(!refused.contains("Pasted"), "{refused}");
    }
    assert!(matches!(
        parse(args(&["mcp"])),
        Ok(Command::CommandHelp { command }) if command == "mcp"
    ));
    assert!(matches!(
        parse(args(&["mcp", "add", "--help"])),
        Ok(Command::CommandHelp { command }) if command == "mcp"
    ));
}

#[test]
fn resume_aliases_accept_at_most_one_session_id() {
    let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    for argv in [["resume", session_id], ["--resume", session_id]] {
        assert!(matches!(
            parse(args(&argv)),
            Ok(Command::Resume { session_id: Some(actual), .. }) if actual == session_id
        ));
    }

    for argv in [
        &["resume", session_id, "extra"][..],
        &["--resume", session_id, "extra"][..],
        &["--resume", "--unknown"][..],
    ] {
        assert!(parse(args(argv)).is_err(), "must reject {argv:?}");
    }
}

#[test]
fn the_private_host_resume_marker_is_distinct_from_embedded_resume() {
    for argv in [&["--host-resume"][..], &["--host-resume", "aaaaaaaa"][..]] {
        assert!(matches!(parse(args(argv)), Ok(Command::HostResume { .. })));
    }
    for argv in [
        &["--host-resume", "--other"][..],
        &["--host-resume", "id", "extra"][..],
    ] {
        assert!(parse(args(argv)).is_err());
    }
}

#[test]
fn placement_flags_only_launch_the_tui_and_preserve_runner_owned_path_bytes() {
    assert!(
        matches!(parse(args(&["--runner", "machine", "--workspace", "/remote/work"])),
        Ok(Command::TuiPlacement { runner: Some(runner), workspace: Some(workspace) })
            if runner == "machine" && workspace == "/remote/work")
    );
    assert!(matches!(
        parse(args(&["--workspace", "/host/work"])),
        Ok(Command::TuiPlacement {
            runner: None,
            workspace: Some(_)
        })
    ));
    for argv in [
        &["--runner"][..],
        &["--workspace"][..],
        &["--runner", "machine", "run", "--", "prompt"][..],
        &["--runner", "machine", "agent"][..],
    ] {
        assert!(parse(args(argv)).is_err(), "{argv:?}");
    }
}

#[test]
fn top_level_init_and_setup_are_absent_but_config_init_remains() {
    for retired in ["init", "setup"] {
        assert_eq!(
            parse(args(&[retired])).unwrap_err(),
            "unknown command; run `plak help` for the command list"
        );
    }
    assert!(matches!(
        parse(args(&["config", "init"])),
        Ok(Command::ConfigInit)
    ));
}

#[test]
fn parser_errors_preserve_useful_categories_without_payloads() {
    assert_eq!(
        parse(args(&["config", "set", "tui.theme"])).unwrap_err(),
        "config set requires a value: plak config set <key> <value>"
    );
    assert_eq!(
        parse(args(&["not-a-command"])).unwrap_err(),
        "unknown command; run `plak help` for the command list"
    );
    assert_eq!(
        parse(args(&["doctor", "--sk-live-secret"])).unwrap_err(),
        "unknown option or unexpected argument; use `plak doctor --help`"
    );
}

#[test]
fn session_delete_abandon_children_is_an_explicit_delete_only_flag() {
    let parsed =
        parse(["session", "delete", "aaaa", "--abandon-children", "--yes"].map(OsString::from));
    assert!(parsed.is_ok(), "{parsed:?}");
    assert!(matches!(
        parsed,
        Ok(Command::SessionDelete {
            yes: true,
            abandon_children: true,
            ..
        })
    ));
    assert!(parse(["session", "show", "aaaa", "--abandon-children"].map(OsString::from)).is_err());
    assert!(parse(["session", "gc", "aaaa", "--abandon-children"].map(OsString::from)).is_err());
}

#[test]
fn clap_long_options_accept_equals_syntax() {
    assert!(matches!(
        parse(args(&[
            "run",
            "--account=codex",
            "--model=gpt-5.3-codex-spark",
            "--",
            "hello"
        ])),
        Ok(Command::Run { model, prompt, .. })
            if model == "gpt-5.3-codex-spark" && prompt == "hello"
    ));
}

#[cfg(not(feature = "maintainer"))]
#[test]
fn product_command_argv_rejects_maintainer_verbs() {
    let rejected: &[&[&str]] = &[
        &["compact", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
        &["fork", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
        &["rebind", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
        &["compatibility", "codex"],
        &["update"],
    ];
    for argv in rejected {
        assert!(
            parse(args(argv)).is_err(),
            "release parser must reject {argv:?}"
        );
    }
}

#[cfg(feature = "maintainer")]
#[test]
#[allow(
    clippy::type_complexity,
    reason = "table of argv slices → predicate is clearer than a macro here"
)]
fn product_command_maintainer_argv_matrix() {
    let cases: &[(&[&str], fn(&Command) -> bool)] = &[
        (&["compatibility", "codex"], |c| {
            matches!(c, Command::CompatibilityNotice(NativeProvider::Codex))
        }),
        (
            &["fork", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
            |c| {
                matches!(
                    c,
                    Command::Fork {
                        confirmed: true,
                        ..
                    }
                )
            },
        ),
        (
            &["compact", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
            |c| {
                matches!(
                    c,
                    Command::Compact {
                        confirmed: true,
                        ..
                    }
                )
            },
        ),
    ];
    for (argv, check) in cases {
        let parsed = parse(args(argv));
        assert!(parsed.is_ok(), "expected Ok for {argv:?}, got {parsed:?}");
        let cmd = parsed.unwrap();
        assert!(check(&cmd), "predicate failed for {argv:?} → {cmd:?}");
    }
    assert!(parse(args(&["update"])).is_err());
}

/// Every remedy string any product surface can print must name a command
/// this build actually has.
///
/// The TUI prints these into the Models readiness header, the Usage
/// not-connected lines, the notes and `/help`. Before this test they were
/// free-form strings, and `plak login openrouter` — an OAuth verb on an
/// API-key account — shipped to operators as advice that only produces a
/// refusal.
#[test]
fn every_tui_remedy_names_a_command_this_build_has() {
    for remedy in crate::product_config::settings::remedy_catalog() {
        let mut tokens = remedy.split_whitespace();
        assert_eq!(
            tokens.next(),
            Some("plak"),
            "a remedy is a command line starting with the binary: {remedy}"
        );
        let argv: Vec<&str> = tokens.collect();
        let parsed = parse(args(&argv));
        assert!(
            parsed.is_ok(),
            "remedy `{remedy}` is not a command this build accepts: {:?}",
            parsed.err()
        );
        // A remedy that only prints its own usage has not told the operator
        // what to run, so `CommandHelp` is as much a failure as a parse
        // error.
        assert!(
            !matches!(parsed, Ok(Command::CommandHelp { .. })),
            "remedy `{remedy}` resolves to a usage page, not an action"
        );
    }
}

/// The canonical verbs are `plak login` / `plak logout` / `plak key`. The
/// legacy `plak auth` tree is retired: it names no product surface and is
/// rejected like any other unknown command.
#[test]
fn the_legacy_auth_tree_is_retired_and_the_canonical_verbs_remain() {
    assert_eq!(
        parse(args(&["auth", "login", "codex"])).unwrap_err(),
        "unknown command; run `plak help` for the command list"
    );
    assert!(parse(args(&["login", "codex"])).is_ok());
    assert!(parse(args(&["logout", "codex"])).is_ok());
    assert!(parse(args(&["key", "set", "openrouter"])).is_ok());
    // The defect this closes: an OAuth verb on an API-key account.
    assert!(parse(args(&["login", "openrouter"])).is_err());
}

#[test]
fn product_argv_rejects_garbage_without_reflecting_payloads() {
    let rejected: &[&[&str]] = &[
        &["not-a-command"],
        &["update"],                     // dropped until public releases exist
        &["login", "openai"],            // API key account, not OAuth
        &["run", "--account", "codex"],  // missing --model and prompt
        &["config", "set", "tui.theme"], // missing value
    ];
    for argv in rejected {
        let err = parse(args(argv)).unwrap_err();
        // Redaction: do not echo huge/secret-looking blobs; errors stay short.
        assert!(err.len() < 500, "error too chatty: {err}");
        assert!(!err.contains("sk-live"), "{err}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Arbitrary short argv tokens never panic and either parse or return stable Err.
    #[test]
    fn arbitrary_short_argv_never_panics(
        tokens in proptest::collection::vec("[a-zA-Z0-9_\\-]{0,16}", 0..6)
    ) {
        let argv: Vec<OsString> = tokens.into_iter().map(OsString::from).collect();
        let _ = parse(argv);
    }
}

/// Every path the operator types is made absolute against the launch
/// directory at the edge; an absolute one is kept, and with no launch
/// directory a relative one is left as typed.
#[test]
fn relative_paths_resolve_against_the_launch_directory() {
    let launch = std::path::Path::new("/work/repo");
    let resolved = |values: &[&str]| parse(args(values)).unwrap().resolved_against(Some(launch));
    assert!(matches!(
        resolved(&["session", "export", "aaaa", "--out", "x.json"]),
        Command::SessionExport { out: Some(out), .. } if out == launch.join("x.json")
    ));
    assert!(matches!(
        resolved(&["session", "import", "/tmp/x.json"]),
        Command::SessionImport { path } if path.as_os_str() == "/tmp/x.json"
    ));
    assert!(matches!(
        resolved(&["session", "import", "in/x.json"]),
        Command::SessionImport { path } if path == launch.join("in/x.json")
    ));
    assert!(matches!(
        parse(args(&["session", "import", "x.json"])).unwrap().resolved_against(None),
        Command::SessionImport { path } if path.as_os_str() == "x.json"
    ));
}
