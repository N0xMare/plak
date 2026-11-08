use super::*;

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
async fn socket_reads_are_attachment_free_and_web_is_gated_before_work()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    let state = root.path().join("state");
    let console = Console::new(
        paths.clone(),
        state.clone(),
        Some(tokio::runtime::Handle::current()),
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
