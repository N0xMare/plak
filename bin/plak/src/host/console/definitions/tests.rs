use super::*;
use crate::home::paths::PlakPaths;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn definitions_withhold_literals_and_etags_cover_comments_and_host_generation() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    std::fs::create_dir_all(paths.user_agents_dir())?;
    std::fs::write(paths.config_file(), "# original comment\n")?;
    let template = AgentTemplate {
        account: "codex".into(),
        model: crate::capabilities::RECOMMENDED_CODEX_MODEL.into(),
        description: "inert description".into(),
        on_mint: Some("echo PRIVATE-HOOK-LITERAL".into()),
        env: BTreeMap::from([("TOKEN".into(), "PRIVATE-ENV-LITERAL".into())]),
        ..AgentTemplate::default()
    };
    std::fs::write(
        paths.user_agents_dir().join("reader.toml"),
        crate::product_config::render_template(&template),
    )?;
    let snapshot = Snapshot::read(&paths, &[7; 16]).map_err(|_| "definition snapshot")?;
    let response = snapshot
        .response(&HostRequest::TemplatesGet {
            id: "reader".into(),
        })
        .map_err(|_| "template read")?;
    let encoded = serde_json::to_string(&response)?;
    assert!(!encoded.contains("PRIVATE-HOOK-LITERAL"));
    assert!(!encoded.contains("PRIVATE-ENV-LITERAL"));
    assert!(encoded.contains("withheld"));
    assert!(encoded.contains("TOKEN"));
    let HostResponse::TemplatesGet { template } = response else {
        return Err("template body".into());
    };
    assert_eq!(template.body.on_mint, ProtectedText::Withheld {});
    assert!(
        template
            .body
            .env
            .iter()
            .all(|entry| entry.value == ProtectedText::Withheld {})
    );
    let same = Snapshot::read(&paths, &[7; 16]).map_err(|_| "same snapshot")?;
    assert_eq!(same.etag, snapshot.etag);
    let other_host = Snapshot::read(&paths, &[8; 16]).map_err(|_| "other host")?;
    assert_ne!(other_host.etag, snapshot.etag);
    std::fs::write(
        paths.config_file(),
        "# comment participates in content identity\n",
    )?;
    let changed = Snapshot::read(&paths, &[7; 16]).map_err(|_| "changed snapshot")?;
    assert_ne!(changed.etag, snapshot.etag);
    Ok(())
}

#[test]
fn definition_reads_do_not_follow_links_or_allocate_past_file_or_scan_bounds() -> TestResult {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    std::fs::create_dir_all(paths.home())?;
    let outside = root.path().join("outside");
    std::fs::write(&outside, "# never admitted\n")?;
    symlink(&outside, paths.config_file())?;
    assert!(files::Files::read(&paths).is_err());
    std::fs::remove_file(paths.config_file())?;
    std::fs::write(paths.config_file(), vec![b'#'; 256 * 1024 + 1])?;
    assert!(matches!(
        files::Files::read(&paths),
        Err(ReadRefusal::Unavailable {
            cause: UnavailableCode::DefinitionTooLarge,
            ..
        })
    ));
    std::fs::remove_file(paths.config_file())?;
    std::fs::create_dir_all(paths.home().join("profiles"))?;
    for index in 0..513 {
        std::fs::write(
            paths
                .home()
                .join("profiles")
                .join(format!("ignored-{index}.txt")),
            "",
        )?;
    }
    assert!(matches!(
        files::Files::read(&paths),
        Err(ReadRefusal::Unavailable {
            cause: UnavailableCode::DependencyLimit,
            ..
        })
    ));
    Ok(())
}

#[test]
fn definition_dependencies_have_an_eight_mib_aggregate_bound() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    std::fs::create_dir_all(paths.user_agents_dir())?;
    let bytes = vec![b'#'; 256 * 1024];
    for index in 0..33 {
        std::fs::write(
            paths
                .user_agents_dir()
                .join(format!("bounded-{index}.toml")),
            &bytes,
        )?;
    }
    assert!(matches!(
        files::Files::read(&paths),
        Err(ReadRefusal::Unavailable {
            cause: UnavailableCode::DependencyLimit,
            ..
        })
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoded_definition_refuses_response_overflow_without_opening_a_session() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    std::fs::create_dir_all(paths.user_agents_dir())?;
    // Literal TOML needs no escaping here; the wire view doubles backslashes.
    let text = format!(
        "account = 'codex'\nmodel = '{}'\ndescription = '{}'\n",
        crate::capabilities::RECOMMENDED_CODEX_MODEL,
        "\\".repeat(140 * 1024)
    );
    std::fs::write(paths.user_agents_dir().join("reader.toml"), text)?;
    let console = super::super::Console::new(
        paths,
        root.path().join("state"),
        Some(tokio::runtime::Handle::current()),
    );
    let context = super::super::HostRequestContext {
        principal: plakous_hosted::host_service::wire::HostPrincipal::Socket(7),
        connection_id: 1,
        host_generation: 1,
    };
    let response = console
        .request(
            context,
            HostRequest::TemplatesGet {
                id: "reader".into(),
            },
        )
        .await
        .map_err(|_| "wire refused")?;
    assert!(matches!(
        response,
        HostResponse::Refused {
            reason: ReadRefusal::Unavailable {
                cause: UnavailableCode::DefinitionTooLarge,
                ..
            }
        }
    ));
    Ok(())
}

#[test]
fn missing_home_stays_missing_and_config_is_the_exact_home_allowlist() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("absent"));
    let snapshot = Snapshot::read(&paths, &[1; 16]).map_err(|_| "missing home")?;
    let HostResponse::ConfigGet { fields, scope, .. } = snapshot
        .response(&HostRequest::ConfigGet {})
        .map_err(|_| "config read")?
    else {
        return Err("config response".into());
    };
    assert_eq!(scope, HomeScope::Home);
    assert_eq!(fields.len(), 25);
    assert!(fields.iter().all(|field| {
        !["tui.", "web.", "tools.", "mcp"]
            .iter()
            .any(|prefix| field.key.starts_with(prefix))
    }));
    assert!(!paths.home().exists());
    for id in ["", "../reader", " reader", "reader ", "a\\b", "a/b"] {
        let Err(ReadRefusal::ValidationFailed { issues }) =
            snapshot.response(&HostRequest::TemplatesGet { id: id.into() })
        else {
            return Err("invalid id was not rejected".into());
        };
        assert_eq!(issues[0].code, ValidationCode::InvalidId);
    }
    Ok(())
}
