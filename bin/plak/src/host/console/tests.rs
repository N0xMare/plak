use super::*;

const DISABLED_SWITCHES: ConfiguredSwitches = ConfiguredSwitches {
    web_accounts_read: false,
    web_config_read: false,
};

fn context(principal: HostPrincipal) -> HostRequestContext {
    HostRequestContext {
        principal,
        connection_id: 7,
        host_generation: 9,
    }
}
fn status(account_id: Option<&str>, limit: Option<u64>) -> HostRequest {
    HostRequest::AccountsStatus {
        account_id: account_id.map(str::to_owned),
        cursor: None,
        limit,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn welcome_and_dispatch_share_independent_startup_switches_without_session_admission()
-> Result<(), Box<dyn std::error::Error>> {
    for accounts in [false, true] {
        for config in [false, true] {
            let root = tempfile::tempdir()?;
            let paths = PlakPaths::explicit(root.path().join("home"));
            let state = root.path().join("state");
            let switches = ConfiguredSwitches {
                web_accounts_read: accounts,
                web_config_read: config,
            };
            let console = Console::new(
                paths.clone(),
                state.clone(),
                Some(tokio::runtime::Handle::current()),
                switches,
            );
            let web = context(HostPrincipal::Web("verified".into()));
            let welcome = console.welcome(&web.principal, &admission_limits());
            assert_eq!(welcome.console.configured_switches, switches);
            assert_eq!(welcome.console.accounts_read.enabled, accounts);
            assert_eq!(welcome.console.config_read.enabled, config);
            let response = console.request(web.clone(), status(None, None)).await;
            assert_eq!(response.is_ok(), accounts);
            let response = console
                .request(web.clone(), HostRequest::ConfigGet {})
                .await;
            assert_eq!(response.is_ok(), config);
            let socket = console.welcome(&HostPrincipal::Socket(7), &admission_limits());
            assert!(socket.console.accounts_read.enabled && socket.console.config_read.enabled);
            assert!(!paths.home().exists());
            assert!(!state.exists());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_reads_are_attachment_free_and_web_is_gated_before_work()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    let state = root.path().join("state");
    let console = Console::new(
        paths.clone(),
        state.clone(),
        Some(tokio::runtime::Handle::current()),
        DISABLED_SWITCHES,
    );
    let socket = context(HostPrincipal::Socket(7));
    let web = context(HostPrincipal::Web("operator".into()));
    assert!(matches!(
        console.request(web, status(None, None)).await,
        Err(RequestRefusal::CapabilityDisabled {
            capability: ConsoleCapability::AccountsRead
        })
    ));
    assert!(!paths.home().exists());
    let HostResponse::AccountsStatus { rows, page, .. } = console
        .request(socket, status(None, None))
        .await
        .map_err(|_| "fixture request refused")?
    else {
        return Err("status response".into());
    };
    assert_eq!(rows.len(), 7, "every default, including compiled-out lanes");
    assert_eq!(page.next, None);
    assert!(
        !paths.home().exists(),
        "no runtime, product, tree or telemetry opened"
    );
    assert!(
        !state.exists(),
        "status never creates a credential namespace"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_selectors_and_page_limits_are_exact_and_refused_before_network()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    let console = Console::new(
        paths,
        root.path().join("state"),
        Some(tokio::runtime::Handle::current()),
        DISABLED_SWITCHES,
    );
    let socket = context(HostPrincipal::Socket(7));
    for alias in [
        "chatgpt",
        "chatgpt/work",
        "supergrok",
        "xai-oauth",
        " codex",
        "codex ",
        "codex/../a",
        "codex/default",
        "codex/Upper",
    ] {
        assert!(accounts::exact(alias).is_err(), "{alias}");
        assert!(matches!(
            console
                .request(socket.clone(), status(Some(alias), None))
                .await
                .map_err(|_| "fixture request refused")?,
            HostResponse::Refused {
                reason: ReadRefusal::ValidationFailed { .. }
            }
        ));
    }
    for limit in [0, 101, u64::MAX] {
        assert!(matches!(
            console
                .request(socket.clone(), status(None, Some(limit)))
                .await
                .map_err(|_| "fixture request refused")?,
            HostResponse::Refused {
                reason: ReadRefusal::ValidationFailed { .. }
            }
        ));
    }
    let selected = accounts::exact("openai/work").map_err(|_| "fixture account refused")?;
    assert_eq!(selected.as_str(), "openai/work");
    Ok(())
}

#[test]
fn raw_invalid_names_consume_the_account_scan_budget() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    let shelf = crate::home::paths::SecretShelf::Api(plakous_hosted::account::Vertical::OpenAi);
    paths.ensure_secret_shelf_dir(shelf)?;
    for index in 0..4 {
        std::fs::write(
            paths
                .secret_shelf_dir(shelf)
                .join(format!("invalid-{index}.not-a-key")),
            "",
        )?;
    }
    let (names, count, cut) = crate::home::secrets::list_shelf_secrets_bounded(&paths, shelf, 2)?;
    assert_eq!(count, 2);
    assert_eq!(names, Vec::<String>::new());
    assert!(cut);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn definitions_and_metadata_need_no_attachment_and_cursors_do_not_cross_read_kinds()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("absent"));
    let state = root.path().join("state");
    let console = Console::new(
        paths.clone(),
        state.clone(),
        Some(tokio::runtime::Handle::current()),
        DISABLED_SWITCHES,
    );
    let socket = context(HostPrincipal::Socket(7));
    assert!(matches!(
        console
            .request(
                context(HostPrincipal::Web("operator".into())),
                HostRequest::ConfigGet {}
            )
            .await,
        Err(RequestRefusal::CapabilityDisabled {
            capability: ConsoleCapability::ConfigRead
        })
    ));
    let response = console
        .request(
            socket.clone(),
            HostRequest::ProfilesList {
                cursor: None,
                limit: Some(1),
            },
        )
        .await
        .map_err(|_| "profiles refused")?;
    let HostResponse::ProfilesList { rows, page, .. } = response else {
        return Err("profiles response".into());
    };
    assert_eq!(rows.len(), 1);
    let profile = console
        .request(
            socket.clone(),
            HostRequest::ProfilesGet {
                id: rows[0].id.clone(),
            },
        )
        .await
        .map_err(|_| "profile refused")?;
    assert!(matches!(profile, HostResponse::ProfilesGet { .. }));
    if let Some(cursor) = page.next {
        let response = console
            .request(
                socket.clone(),
                HostRequest::TemplatesList {
                    cursor: Some(cursor),
                    limit: Some(1),
                },
            )
            .await
            .map_err(|_| "cursor refused outside product")?;
        assert!(matches!(
            response,
            HostResponse::Refused {
                reason: ReadRefusal::ValidationFailed { .. }
            }
        ));
    }
    let response = console
        .request(
            socket.clone(),
            HostRequest::TemplatesList {
                cursor: None,
                limit: Some(1),
            },
        )
        .await
        .map_err(|_| "templates refused")?;
    let HostResponse::TemplatesList { rows, .. } = response else {
        return Err("templates response".into());
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        console
            .request(
                socket.clone(),
                HostRequest::TemplatesGet {
                    id: rows[0].id.clone()
                }
            )
            .await
            .map_err(|_| "template refused")?,
        HostResponse::TemplatesGet { .. }
    ));
    assert!(matches!(
        console
            .request(socket.clone(), HostRequest::ConfigGet {})
            .await
            .map_err(|_| "config refused")?,
        HostResponse::ConfigGet { .. }
    ));
    assert_metadata(&console, socket).await?;
    assert!(!paths.home().exists());
    assert!(!state.exists());
    Ok(())
}

async fn assert_metadata(
    console: &Arc<Console>,
    socket: HostRequestContext,
) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = console
        .request(
            socket,
            HostRequest::ConsoleMetadataGet {
                account_id: None,
                cursor: None,
                limit: None,
            },
        )
        .await
        .map_err(|_| "metadata refused")?;
    let HostResponse::ConsoleMetadata {
        settings,
        catalog,
        needs_setup,
        ..
    } = metadata
    else {
        return Err("metadata response".into());
    };
    assert_eq!(settings.len(), 25);
    assert_eq!(catalog.rows, Vec::new());
    assert_eq!(catalog.account_id, None);
    assert!(needs_setup);
    Ok(())
}
