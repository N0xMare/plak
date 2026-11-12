use super::*;
use crate::home::paths::PlakPaths;
use plakd_wire_types::console::{
    ConfigChange, ConfiguredSwitches, ProfileBody, SettingValue, TemplateChange,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture() -> (tempfile::TempDir, Arc<Console>) {
    let (root, paths) = PlakPaths::isolated_test_home();
    let console = Console::new(
        paths,
        root.path().join("state"),
        Some(tokio::runtime::Handle::current()),
        ConfiguredSwitches {
            web_accounts_read: false,
            web_config_read: false,
            web_credentials_write: false,
            web_config_write: false,
            web_files_read: false,
            web_terminal: false,
        },
    );
    (root, console)
}
fn owner(connection: u64) -> HostRequestContext {
    HostRequestContext {
        principal: plakous_hosted::host_service::wire::HostPrincipal::Socket(7),
        connection_id: connection,
        host_generation: 1,
        request_id: 1,
    }
}
fn revision(console: &Console) -> String {
    definitions::Snapshot::read(&console.paths, &console.definition_nonce)
        .expect("snapshot")
        .etag
}
fn consent(console: &Console, connection: u64, action: OperatorAction) -> String {
    let Ok(HostResponse::OperatorPrepared { consent }) = console.prepare(owner(connection), action)
    else {
        panic!("prepare")
    };
    let Ok(HostResponse::OperatorConfirmed { confirmation, .. }) =
        console.confirm(&owner(connection), &consent.nonce)
    else {
        panic!("confirm")
    };
    confirmation
}
fn config(
    console: &Console,
    connection: u64,
    if_match: String,
    key: &str,
    value: SettingValue,
) -> HostRequest {
    let action = OperatorAction::ConfigPatch {
        if_match: if_match.clone(),
        keys: vec![key.into()],
    };
    HostRequest::ConfigPatch {
        if_match,
        changes: vec![ConfigChange {
            key: key.into(),
            value,
        }],
        confirmation: consent(console, connection, action),
    }
}
fn template(console: &Console, id: &str, changes: Vec<TemplateChange>) -> HostRequest {
    let if_match = revision(console);
    HostRequest::TemplatePut {
        id: id.into(),
        if_match: if_match.clone(),
        changes,
        confirmation: consent(
            console,
            1,
            OperatorAction::TemplatePut {
                id: id.into(),
                if_match,
            },
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_metadata_follows_effective_write_capability() -> TestResult {
    let (_root, console) = fixture();
    for (socket, web_write) in [(true, false), (false, false), (false, true)] {
        let console = Console::new(
            console.paths.clone(),
            console.state.clone(),
            Some(tokio::runtime::Handle::current()),
            ConfiguredSwitches {
                web_accounts_read: false,
                web_credentials_write: false,
                web_config_read: true,
                web_config_write: web_write,
                web_files_read: web_write,
                web_terminal: false,
            },
        );
        let mut context = owner(1);
        if !socket {
            context.principal = plakous_hosted::host_service::wire::HostPrincipal::Web(
                "operator@example.test".into(),
            );
        }
        let Ok(HostResponse::ConsoleMetadata { settings, .. }) = console
            .request(
                context,
                HostRequest::ConsoleMetadataGet {
                    account_id: None,
                    cursor: None,
                    limit: None,
                },
            )
            .await
        else {
            return Err("metadata response".into());
        };
        assert_ne!(settings.len(), 0);
        assert!(
            settings
                .iter()
                .all(|row| row.read_only != (socket || web_write))
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_conflicts_detect_other_clients_cli_and_external_comment_edits() -> TestResult {
    let (_root, console) = fixture();
    std::fs::write(
        console.paths.config_file(),
        "# retained\n[turn]\nauto_continue = true # inline\n[tui]\ntheme = 'default-light' # untouched\n",
    )?;
    let etag = revision(&console);
    let first = config(
        &console,
        1,
        etag.clone(),
        "turn.auto_continue",
        SettingValue::Bool(false),
    );
    let stale = config(
        &console,
        2,
        etag,
        "turn.auto_continue",
        SettingValue::Bool(true),
    );
    assert!(matches!(
        console.request(owner(1), first).await,
        Ok(HostResponse::Saved {
            published_durability_uncertain: false,
            ..
        })
    ));
    assert!(matches!(
        console.request(owner(2), stale).await,
        Ok(HostResponse::Refused {
            reason: ReadRefusal::Conflict { .. }
        })
    ));
    let bytes = std::fs::read_to_string(console.paths.config_file())?;
    assert!(
        bytes.contains("# retained") && bytes.contains("# inline") && bytes.contains("# untouched")
    );
    assert!(bytes.contains("theme = 'default-light'"));
    let stale_cli = config(
        &console,
        1,
        revision(&console),
        "turn.auto_continue",
        SettingValue::Bool(true),
    );
    crate::product_config::ProductConfig::update_user(&console.paths, |current| {
        current.set_key("turn.max_model_rounds", "12")
    })
    .map_err(ConfigSaveError::into_message)?;
    assert!(matches!(
        console.request(owner(1), stale_cli).await,
        Ok(HostResponse::Refused {
            reason: ReadRefusal::Conflict { .. }
        })
    ));
    let bytes = std::fs::read_to_string(console.paths.config_file())?;
    let request = config(
        &console,
        1,
        revision(&console),
        "turn.auto_continue",
        SettingValue::Bool(true),
    );
    std::fs::write(
        console.paths.config_file(),
        format!("{bytes}\n# external edit\n"),
    )?;
    let external = std::fs::read(console.paths.config_file())?;
    assert!(matches!(
        console.request(owner(1), request).await,
        Ok(HostResponse::Refused {
            reason: ReadRefusal::Conflict { .. }
        })
    ));
    assert_eq!(std::fs::read(console.paths.config_file())?, external);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_edit_during_validation_is_rechecked_before_publication() -> TestResult {
    let (_root, console) = fixture();
    let request = config(
        &console,
        1,
        revision(&console),
        "turn.auto_continue",
        SettingValue::Bool(false),
    );
    let file = console.paths.config_file();
    let result = crate::product_config::console::with_write_lock(&console.paths, || {
        console.publish_config_with_hook(&request, || {
            std::fs::write(&file, "# external during validation\n").expect("editor");
        })
    })
    .map_err(ConfigSaveError::into_message)?;
    assert!(matches!(result, Err(ReadRefusal::Conflict { .. })));
    assert_eq!(
        std::fs::read_to_string(file)?,
        "# external during validation\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_values_and_forbidden_fields_leave_all_home_bytes_unchanged() -> TestResult {
    let (_root, console) = fixture();
    std::fs::write(console.paths.config_file(), "# keep\n")?;
    let before = std::fs::read(console.paths.config_file())?;
    let request = config(
        &console,
        1,
        revision(&console),
        "turn.max_model_rounds",
        SettingValue::Integer(-1),
    );
    assert!(matches!(
        console.request(owner(1), request).await,
        Ok(HostResponse::Refused {
            reason: ReadRefusal::ValidationFailed { .. }
        })
    ));
    for key in [
        "web.config_write",
        "web.allow",
        "tui.theme",
        "mcp_servers.example.command",
        "runners.example.command",
        "tools.exec",
        "../config",
    ] {
        assert!(matches!(
            console.prepare(
                owner(1),
                OperatorAction::ConfigPatch {
                    if_match: revision(&console),
                    keys: vec![key.into()]
                }
            ),
            Ok(HostResponse::Refused { .. })
        ));
    }
    assert_eq!(std::fs::read(console.paths.config_file())?, before);
    Ok(())
}

async fn publish_named_worker_profile(
    console: Arc<Console>,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let create = template(
        &console,
        "worker",
        vec![
            TemplateChange::Account {
                value: "codex/work".into(),
            },
            TemplateChange::Model {
                value: "gpt-5.6-luna".into(),
            },
            TemplateChange::Description {
                value: "scoped work".into(),
            },
        ],
    );
    assert!(matches!(
        console.request(owner(1), create).await,
        Ok(HostResponse::Saved { .. })
    ));
    let template_path = console.paths.user_agents_dir().join("worker.toml");
    let original = std::fs::read_to_string(&template_path)?;
    std::fs::write(
        &template_path,
        format!("# template comment\n{original}\n# footer\n"),
    )?;
    let if_match = revision(&console);
    let profile = HostRequest::ProfilePut {
        id: "work".into(),
        body: ProfileBody {
            root: "worker".into(),
            working_set: vec!["worker".into()],
        },
        if_match: if_match.clone(),
        confirmation: consent(
            &console,
            1,
            OperatorAction::ProfilePut {
                id: "work".into(),
                if_match,
            },
        ),
    };
    assert!(matches!(
        console.request(owner(1), profile).await,
        Ok(HostResponse::Saved { .. })
    ));
    Ok(template_path)
}

async fn publish_work_defaults(console: Arc<Console>) -> TestResult {
    let if_match = revision(&console);
    let default = HostRequest::ConfigPatch {
        changes: vec![
            ConfigChange {
                key: "root.agent".into(),
                value: SettingValue::String("worker".into()),
            },
            ConfigChange {
                key: "roster.working_set".into(),
                value: SettingValue::Strings(vec!["worker".into()]),
            },
        ],
        if_match: if_match.clone(),
        confirmation: consent(
            &console,
            1,
            OperatorAction::ConfigPatch {
                if_match,
                keys: vec!["root.agent".into(), "roster.working_set".into()],
            },
        ),
    };
    assert!(matches!(
        console.request(owner(1), default).await,
        Ok(HostResponse::Saved { .. })
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn templates_profiles_and_defaults_publish_in_separate_validated_steps() -> TestResult {
    let (_root, console) = fixture();
    let template_path = publish_named_worker_profile(Arc::clone(&console)).await?;
    publish_work_defaults(Arc::clone(&console)).await?;
    let frozen = definitions::Snapshot::read(&console.paths, &console.definition_nonce)
        .map_err(|_| "snapshot")?
        .config;
    let frozen_roster =
        crate::roster_input::snapshot_from_config(&frozen, "codex/work", "gpt-5.6-luna", None);
    let update = template(
        &console,
        "worker",
        vec![TemplateChange::Description {
            value: "new genesis only".into(),
        }],
    );
    assert!(matches!(
        console.request(owner(1), update).await,
        Ok(HostResponse::Saved { .. })
    ));
    let edited = std::fs::read_to_string(&template_path)?;
    assert!(edited.contains("# template comment") && edited.contains("# footer"));
    assert_eq!(frozen.templates["worker"].description, "scoped work");
    let after = definitions::Snapshot::read(&console.paths, &console.definition_nonce)
        .map_err(|_| "snapshot")?
        .config;
    let new_roster =
        crate::roster_input::snapshot_from_config(&after, "codex/work", "gpt-5.6-luna", None);
    let name = plakous_hosted::agent_tree_protocol::TemplateName::parse("worker")?;
    assert_eq!(
        frozen_roster
            .binding(&name)
            .ok_or("frozen card")?
            .description,
        "scoped work"
    );
    assert_eq!(
        new_roster.binding(&name).ok_or("new card")?.description,
        "new genesis only"
    );
    assert_eq!(
        new_roster.binding(&name).ok_or("named selection")?.account,
        "codex/work"
    );
    assert_eq!(
        definitions::Snapshot::read(&console.paths, &console.definition_nonce)
            .map_err(|_| "snapshot")?
            .config
            .templates["worker"]
            .description,
        "new genesis only"
    );
    // A new missing spawn reference cannot break the already published profile.
    let update = template(
        &console,
        "worker",
        vec![TemplateChange::Spawns {
            value: vec!["missing".into()],
        }],
    );
    assert!(matches!(
        console.request(owner(1), update).await,
        Ok(HostResponse::Refused { .. })
    ));
    assert_eq!(std::fs::read_to_string(&template_path)?, edited);
    assert!(matches!(
        console.prepare(
            owner(1),
            OperatorAction::TemplatePut {
                id: "../escape".into(),
                if_match: revision(&console)
            }
        ),
        Ok(HostResponse::Refused { .. })
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builtin_profiles_are_read_only_and_definition_links_are_not_followed() -> TestResult {
    let (_root, console) = fixture();
    let if_match = revision(&console);
    let request = HostRequest::ProfilePut {
        id: "codex-default".into(),
        if_match: if_match.clone(),
        body: ProfileBody {
            root: "missing".into(),
            working_set: Vec::new(),
        },
        confirmation: consent(
            &console,
            1,
            OperatorAction::ProfilePut {
                id: "codex-default".into(),
                if_match,
            },
        ),
    };
    assert!(matches!(
        console.request(owner(1), request).await,
        Ok(HostResponse::Refused {
            reason: ReadRefusal::ValidationFailed { .. }
        })
    ));
    let outside = console
        .paths
        .home()
        .parent()
        .ok_or("parent")?
        .join("outside.toml");
    std::fs::write(&outside, "# unchanged outside\n")?;
    std::os::unix::fs::symlink(
        &outside,
        console.paths.user_agents_dir().join("linked.toml"),
    )?;
    let request = template(
        &console,
        "linked",
        vec![
            TemplateChange::Account {
                value: "codex".into(),
            },
            TemplateChange::Model {
                value: "gpt-5.6-luna".into(),
            },
        ],
    );
    assert!(matches!(
        console.request(owner(1), request).await,
        Ok(HostResponse::Refused { .. })
    ));
    assert_eq!(std::fs::read_to_string(outside)?, "# unchanged outside\n");
    Ok(())
}
