//! MCP authorization against the scripted authorization server: discovery,
//! registration, the local, paste-back and device logins, logout, and the
//! credentials of a running server.

use std::sync::Arc;
use std::time::Duration;

use plakous_mcp::{
    McpErrorKind, McpHttpEndpoint, McpLocalRetirement, McpLoginOptions, McpLogoutOutcome,
    McpOAuthSettings, McpOAuthStatus, McpPool, McpRemoteLogin, McpServerSpec, McpToolPack,
    McpToolSelection, SensitiveBytes,
};
use plakous_mcp_fake_server::http::{HttpFixture, HttpGate, serve_http};
use plakous_mcp_fake_server::oauth::{
    CallbackIssuer, OAuthConfig, OAuthGate, ResourceMetadataAt, ServerMetadataAt, browse,
};
use plakous_mcp_fake_server::{FakeServer, Style};
use plakous_tools::{
    FrozenToolRegistry, McpServerName, ToolOutcomeKind, ToolPack, ToolRegistryBuilder,
    register_packs,
};

mod common;

use common::{TestResult, Workspace, execute, workspace};

struct Setup {
    fixture: HttpFixture,
    gate: Arc<OAuthGate>,
    calls: Arc<plakous_mcp_fake_server::Observed>,
    home: tempfile::TempDir,
}

async fn setup(config: OAuthConfig) -> TestResult<Setup> {
    let gate = Arc::new(OAuthGate::new(config));
    let server = FakeServer::new(Style::Discover, Arc::new(|| {}));
    let calls = server.observed();
    let fixture = serve_http(
        server,
        HttpGate {
            oauth: Some(Arc::clone(&gate)),
            ..HttpGate::default()
        },
    )
    .await?;
    let home = tempfile::tempdir()?;
    std::fs::create_dir(home.path().join("remote"))?;
    std::fs::set_permissions(
        home.path().join("remote"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    Ok(Setup {
        fixture,
        gate,
        calls,
        home,
    })
}

impl Setup {
    fn settings(&self) -> McpOAuthSettings {
        McpOAuthSettings::new(self.home.path().join("remote").join("credential.json"))
            .with_login_remedy(Arc::new(|scope| match scope {
                Some(scope) => format!("run login remote --scope '{scope}'"),
                None => "run login remote".to_owned(),
            }))
    }

    /// Token requests of the refresh grant.
    fn refreshes(&self) -> Vec<plakous_mcp_fake_server::oauth::AuthRequest> {
        self.gate
            .requests_to("/token")
            .into_iter()
            .filter(|request| request.field("grant_type") == Some("refresh_token"))
            .collect()
    }

    fn spec(&self, settings: McpOAuthSettings) -> TestResult<McpServerSpec> {
        Ok(McpServerSpec::http(
            McpServerName::new("remote")?,
            McpHttpEndpoint::new(&self.fixture.url())?.with_oauth(settings)?,
        )
        .with_startup_timeout(Duration::from_secs(20))
        .with_call_timeout(Duration::from_secs(20)))
    }
}

/// Runs a local login to completion, the scripted browser following the
/// authorization redirect to the loopback callback.
async fn local_login(pool: &McpPool, spec: &McpServerSpec) -> TestResult<McpOAuthStatus> {
    let local = pool
        .login(spec, McpLoginOptions::new())
        .await?
        .local()
        .await?;
    let url = local.authorization_url().to_owned();
    let browser = tokio::spawn(async move {
        let (_, location) = browse(&url).await?;
        let location = location.ok_or("no redirect")?;
        browse(&location).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let status = local.complete(Duration::from_secs(10)).await?;
    browser.await?.map_err(|error| error.to_string())?;
    Ok(status)
}

fn logged_in(status: &McpOAuthStatus) -> bool {
    matches!(
        status,
        McpOAuthStatus::LoggedIn {
            refreshable: true,
            ..
        }
    )
}

#[tokio::test]
async fn a_local_login_discovers_registers_and_binds_the_token_to_the_resource() -> TestResult {
    let setup = setup(OAuthConfig::default()).await?;
    let spec = setup.spec(setup.settings())?;
    let pool = McpPool::new();
    let login = pool.login(&spec, McpLoginOptions::new()).await?;
    assert_eq!(login.issuer(), setup.gate.issuer());
    assert_eq!(login.scope(), Some("mcp:tools offline_access"));
    drop(login);
    let status = local_login(&pool, &spec).await?;
    assert!(logged_in(&status), "{status:?}");
    assert_eq!(spec.oauth_status()?, Some(status));

    let resource = setup.fixture.url();
    let registered = setup.gate.requests_to("/register-body");
    let authorize = setup.gate.requests_to("/authorize");
    let token = setup.gate.requests_to("/token");
    assert_eq!(authorize.len(), 1);
    let redirect = authorize[0].field("redirect_uri").ok_or("redirect")?;
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"));
    assert_eq!(
        registered
            .last()
            .and_then(|request| request.field("redirect_uris")),
        Some(format!("[\"{redirect}\"]").as_str())
    );
    assert_eq!(authorize[0].field("code_challenge_method"), Some("S256"));
    assert_eq!(authorize[0].field("resource"), Some(resource.as_str()));
    assert_eq!(authorize[0].field("client_id"), Some("dyn-1"));
    assert_eq!(token.len(), 1);
    assert_eq!(token[0].field("resource"), Some(resource.as_str()));
    assert_eq!(token[0].field("grant_type"), Some("authorization_code"));
    // No token ever reaches the metadata, registration or authorize endpoints.
    assert!(
        setup
            .gate
            .requests()
            .iter()
            .all(|request| request.authorization.is_none()),
        "a credential reached the authorization server"
    );
    Ok(())
}

#[tokio::test]
async fn discovery_falls_back_through_the_well_known_urls_in_order() -> TestResult {
    for (resource_at, server_at, expected) in [
        (
            ResourceMetadataAt::PathWellKnown,
            ServerMetadataAt::OidcAppended,
            vec![
                "/.well-known/oauth-protected-resource/mcp",
                "/.well-known/oauth-authorization-server/tenant",
                "/.well-known/openid-configuration/tenant",
                "/tenant/.well-known/openid-configuration",
            ],
        ),
        (
            ResourceMetadataAt::RootWellKnown,
            ServerMetadataAt::OidcInserted,
            vec![
                "/.well-known/oauth-protected-resource/mcp",
                "/.well-known/oauth-protected-resource",
                "/.well-known/oauth-authorization-server/tenant",
                "/.well-known/openid-configuration/tenant",
            ],
        ),
    ] {
        let setup = setup(OAuthConfig {
            resource_metadata: resource_at,
            server_metadata: server_at,
            issuer_path: "/tenant".to_owned(),
            ..OAuthConfig::default()
        })
        .await?;
        let login = McpPool::new()
            .login(&setup.spec(setup.settings())?, McpLoginOptions::new())
            .await?;
        assert_eq!(login.issuer(), setup.gate.issuer());
        let paths = setup
            .gate
            .requests()
            .into_iter()
            .map(|request| request.path)
            .collect::<Vec<_>>();
        assert_eq!(paths, expected, "{resource_at:?} {server_at:?}");
    }
    Ok(())
}

#[tokio::test]
async fn discovery_refuses_another_resource_and_a_server_without_pkce() -> TestResult {
    let foreign = setup(OAuthConfig {
        resource: Some("/mcp/elsewhere".to_owned()),
        ..OAuthConfig::default()
    })
    .await?;
    let refused = McpPool::new()
        .login(&foreign.spec(foreign.settings())?, McpLoginOptions::new())
        .await
        .err()
        .ok_or("another resource was accepted")?;
    assert!(
        refused.message().contains("names another resource"),
        "{refused}"
    );

    let no_pkce = setup(OAuthConfig {
        pkce: false,
        ..OAuthConfig::default()
    })
    .await?;
    let refused = McpPool::new()
        .login(&no_pkce.spec(no_pkce.settings())?, McpLoginOptions::new())
        .await
        .err()
        .ok_or("a server without PKCE was accepted")?;
    assert!(
        refused.message().contains("does not advertise PKCE S256"),
        "{refused}"
    );
    assert!(no_pkce.gate.requests_to("/register").is_empty());

    // An origin-wide audience must not widen a path-specific server.
    let prefix = setup(OAuthConfig {
        resource: Some("/".to_owned()),
        ..OAuthConfig::default()
    })
    .await?;
    assert!(
        McpPool::new()
            .login(&prefix.spec(prefix.settings())?, McpLoginOptions::new())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn an_issuer_mismatch_discards_the_callback_before_any_token_request() -> TestResult {
    for (issuer, advertised) in [
        (CallbackIssuer::Wrong, false),
        (CallbackIssuer::Absent, true),
    ] {
        let setup = setup(OAuthConfig {
            callback_issuer: issuer,
            advertise_iss: advertised,
            ..OAuthConfig::default()
        })
        .await?;
        let refused = local_login(&McpPool::new(), &setup.spec(setup.settings())?)
            .await
            .err()
            .ok_or("a mismatched issuer was accepted")?;
        assert!(
            refused
                .to_string()
                .contains("did not come from the expected authorization server"),
            "{refused}"
        );
        assert!(setup.gate.requests_to("/token").is_empty(), "{issuer:?}");
    }
    Ok(())
}

#[tokio::test]
async fn a_remote_login_takes_the_pasted_callback_url_or_the_device_grant() -> TestResult {
    let paste = setup(OAuthConfig::default()).await?;
    let spec = paste.spec(paste.settings())?;
    let pool = McpPool::new();
    let McpRemoteLogin::Paste(login) = pool
        .login(&spec, McpLoginOptions::new())
        .await?
        .remote()
        .await?
    else {
        return Err("a server without the device grant offered it".into());
    };
    let (_, location) = browse(login.authorization_url()).await?;
    let location = location.ok_or("no redirect")?;
    // The redirect is not served: nothing listens on a remote login's port.
    assert!(browse(&location).await.is_err());
    assert!(logged_in(&login.complete(&location).await?));

    let device = setup(OAuthConfig {
        device: true,
        ..OAuthConfig::default()
    })
    .await?;
    let spec = device.spec(device.settings())?;
    let McpRemoteLogin::Device(login) = pool
        .login(&spec, McpLoginOptions::new())
        .await?
        .remote()
        .await?
    else {
        return Err("the device grant was not used".into());
    };
    assert_eq!(login.user_code(), "WDJB-MJHT");
    assert!(logged_in(&login.complete().await?));
    let device_request = &device.gate.requests_to("/device")[0];
    assert_eq!(
        device_request.field("resource"),
        Some(device.fixture.url().as_str())
    );
    Ok(())
}

#[tokio::test]
async fn logout_revokes_the_refresh_token_or_retires_locally() -> TestResult {
    let setup = setup(OAuthConfig::default()).await?;
    let spec = setup.spec(setup.settings())?;
    let pool = McpPool::new();
    local_login(&pool, &spec).await?;
    assert_eq!(pool.logout(&spec, false).await?, McpLogoutOutcome::Revoked);
    let revoked = setup.gate.requests_to("/revoke");
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].field("token_type_hint"), Some("refresh_token"));
    assert!(
        revoked[0]
            .field("token")
            .is_some_and(|token| token.starts_with("rt-"))
    );
    assert_eq!(spec.oauth_status()?, Some(McpOAuthStatus::NotLoggedIn));
    assert_eq!(
        pool.logout(&spec, false).await?,
        McpLogoutOutcome::NotLoggedIn
    );

    local_login(&pool, &spec).await?;
    assert_eq!(
        pool.logout(&spec, true).await?,
        McpLogoutOutcome::RetiredLocally(McpLocalRetirement::Requested)
    );
    assert_eq!(setup.gate.requests_to("/revoke").len(), 1);

    let unrevocable = self::setup(OAuthConfig {
        revocation: false,
        ..OAuthConfig::default()
    })
    .await?;
    let spec = unrevocable.spec(unrevocable.settings())?;
    local_login(&pool, &spec).await?;
    assert_eq!(
        pool.logout(&spec, false).await?,
        McpLogoutOutcome::RetiredLocally(McpLocalRetirement::NoRevocationEndpoint)
    );
    Ok(())
}

#[tokio::test]
async fn a_preregistered_client_authenticates_with_its_secret_only_at_its_issuer() -> TestResult {
    let setup = setup(OAuthConfig {
        registration: false,
        confidential_client: Some(("client-9".to_owned(), "s3cret".to_owned())),
        ..OAuthConfig::default()
    })
    .await?;
    let settings = setup
        .settings()
        .with_issuer(&setup.gate.issuer())?
        .with_client(
            "client-9",
            Some(Arc::new(|| Ok(SensitiveBytes::new(b"s3cret".to_vec())))),
        )?;
    let spec = setup.spec(settings)?;
    let pool = McpPool::new();
    assert!(logged_in(&local_login(&pool, &spec).await?));
    let token = &setup.gate.requests_to("/token")[0];
    assert!(
        token
            .authorization
            .as_deref()
            .is_some_and(|value| value.starts_with("Basic "))
    );
    assert_eq!(token.field("client_secret"), None);

    // Without the pin a pre-registered client is refused at definition.
    let unpinned = setup.settings().with_client("client-9", None)?;
    assert!(
        McpHttpEndpoint::new(&setup.fixture.url())?
            .with_oauth(unpinned)
            .is_err()
    );
    // Without registration or a client, a login names the remedy.
    let bare = setup.spec(setup.settings())?;
    let refused = pool
        .login(&bare, McpLoginOptions::new())
        .await?
        .local()
        .await
        .err()
        .ok_or("a login without any registration path started")?;
    assert!(
        refused.message().contains("offers no client registration"),
        "{refused}"
    );
    Ok(())
}

async fn registry(
    pool: &McpPool,
    spec: McpServerSpec,
    workspace: &Workspace,
) -> TestResult<FrozenToolRegistry> {
    let lease = pool.lease(spec, Arc::clone(&workspace.environment)).await?;
    let listing = lease.freeze(&McpToolSelection::All).await?;
    let pack: Arc<dyn ToolPack> = Arc::new(McpToolPack::new(lease, &listing));
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&[pack], &mut builder)?;
    Ok(builder.freeze())
}

async fn echo(
    registry: &FrozenToolRegistry,
    workspace: &Workspace,
) -> TestResult<plakous_tools::ToolOutcome> {
    execute(
        registry,
        Arc::clone(&workspace.environment),
        "mcp__remote__echo",
        r#"{"text":"hi"}"#,
    )
    .await
}

#[tokio::test]
async fn calls_carry_the_token_only_to_the_endpoint_and_refresh_it_before_expiry() -> TestResult {
    // A token that expires within the five-minute lead refreshes before
    // every request; each refresh spends the rotated refresh token.
    let setup = setup(OAuthConfig {
        access_lifetime: 60,
        ..OAuthConfig::default()
    })
    .await?;
    let spec = setup.spec(setup.settings())?;
    let pool = McpPool::new();
    local_login(&pool, &spec).await?;
    let workspace = workspace()?;
    let registry = registry(&pool, spec.clone(), &workspace).await?;
    for _ in 0..2 {
        assert_eq!(
            echo(&registry, &workspace).await?.kind(),
            ToolOutcomeKind::Success
        );
    }
    assert_eq!(setup.calls.calls(), ["echo", "echo"]);

    let refreshes = setup.refreshes();
    assert!(refreshes.len() >= 3, "{}", refreshes.len());
    let resource = setup.fixture.url();
    let mut spent = Vec::new();
    for request in &refreshes {
        assert_eq!(request.field("resource"), Some(resource.as_str()));
        let token = request.field("refresh_token").ok_or("no refresh token")?;
        assert!(
            !spent.contains(&token.to_owned()),
            "a refresh token was reused"
        );
        spent.push(token.to_owned());
    }
    // Tokens reach the MCP endpoint and nothing else.
    let observed = setup.fixture.observed();
    assert!(observed.requests().iter().any(|request| {
        request.refused.is_none()
            && request
                .header("authorization")
                .is_some_and(|value| value.starts_with("Bearer at-"))
    }));
    assert!(
        setup
            .gate
            .requests()
            .iter()
            .all(|request| request.authorization.is_none()),
        "a token reached the authorization server"
    );
    assert!(matches!(
        spec.oauth_status()?,
        Some(McpOAuthStatus::LoggedIn { .. })
    ));
    pool.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_rejected_token_is_renewed_once_and_the_call_is_not_resent() -> TestResult {
    let setup = setup(OAuthConfig::default()).await?;
    let spec = setup.spec(setup.settings())?;
    let pool = McpPool::new();
    local_login(&pool, &spec).await?;
    let workspace = workspace()?;
    let registry = registry(&pool, spec, &workspace).await?;
    assert!(setup.refreshes().is_empty());

    setup.gate.reject_access_tokens();
    let rejected = echo(&registry, &workspace).await?;
    assert_eq!(rejected.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        rejected.joined_text(),
        "mcp server remote: not executed: credentials were rejected and have been renewed; call again"
    );
    assert_eq!(
        setup.calls.calls(),
        Vec::<String>::new(),
        "the call was re-sent"
    );
    assert_eq!(setup.refreshes().len(), 1);

    assert_eq!(
        echo(&registry, &workspace).await?.kind(),
        ToolOutcomeKind::Success
    );
    assert_eq!(setup.calls.calls(), ["echo"]);
    assert_eq!(setup.refreshes().len(), 1);
    pool.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_start_renews_a_rejected_token_and_retries_once() -> TestResult {
    let setup = setup(OAuthConfig::default()).await?;
    let spec = setup.spec(setup.settings())?;
    let pool = McpPool::new();
    local_login(&pool, &spec).await?;
    let workspace = workspace()?;
    setup.gate.reject_access_tokens();
    let lease = pool
        .lease(spec.clone(), Arc::clone(&workspace.environment))
        .await?;
    assert_eq!(setup.refreshes().len(), 1);
    drop(lease);
    pool.shutdown().await;

    // A grant the server revoked cannot renew: the login is retired and the
    // start names the remedy.
    let pool = McpPool::new();
    setup.gate.reject_access_tokens();
    setup.gate.reject_refresh_tokens();
    let refused = pool
        .lease(spec.clone(), Arc::clone(&workspace.environment))
        .await
        .err()
        .ok_or("a revoked grant started")?;
    // An authorization error: its text carries the one login remedy.
    assert_eq!(refused.kind(), McpErrorKind::Authorization);
    assert!(
        refused.message().contains("invalid_grant")
            && refused.message().ends_with("; run login remote"),
        "{refused}"
    );
    assert_eq!(spec.oauth_status()?, Some(McpOAuthStatus::NotLoggedIn));
    Ok(())
}

#[tokio::test]
async fn insufficient_scope_names_the_step_up_login_and_nothing_runs() -> TestResult {
    let setup = setup(OAuthConfig::default()).await?;
    let spec = setup.spec(setup.settings())?;
    let pool = McpPool::new();
    local_login(&pool, &spec).await?;
    let workspace = workspace()?;
    let registry = registry(&pool, spec, &workspace).await?;
    setup.gate.require_scope(Some("mcp:admin"));
    let refused = echo(&registry, &workspace).await?;
    assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        refused.joined_text(),
        "mcp server remote needs scope \"mcp:tools offline_access mcp:admin\"; \
         run login remote --scope 'mcp:tools offline_access mcp:admin'; the call did not run"
    );
    assert_eq!(setup.calls.calls(), Vec::<String>::new());
    assert!(setup.refreshes().is_empty(), "a step-up is not a refresh");
    pool.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_refresh_the_server_fails_keeps_the_login_for_the_next_one() -> TestResult {
    let setup = setup(OAuthConfig {
        access_lifetime: 60,
        ..OAuthConfig::default()
    })
    .await?;
    let spec = setup.spec(setup.settings())?;
    let pool = McpPool::new();
    local_login(&pool, &spec).await?;
    let workspace = workspace()?;
    setup.gate.fail_token_requests(1);
    let refused = pool
        .lease(spec.clone(), Arc::clone(&workspace.environment))
        .await
        .err()
        .ok_or("a failed refresh started")?;
    assert!(
        refused.message().contains("could not renew its login")
            && refused.message().contains("HTTP 503"),
        "{refused}"
    );
    assert!(matches!(
        spec.oauth_status()?,
        Some(McpOAuthStatus::LoggedIn { .. })
    ));
    let registry = registry(&pool, spec, &workspace).await?;
    assert_eq!(
        echo(&registry, &workspace).await?.kind(),
        ToolOutcomeKind::Success
    );
    pool.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_start_refuses_without_a_login_for_this_server() -> TestResult {
    let setup = setup(OAuthConfig::default()).await?;
    let spec = setup.spec(setup.settings())?;
    let pool = McpPool::new();
    let workspace = workspace()?;
    let refused = pool
        .lease(spec.clone(), Arc::clone(&workspace.environment))
        .await
        .err()
        .ok_or("a server without a login started")?;
    assert_eq!(refused.kind(), McpErrorKind::Authorization);
    assert_eq!(
        refused.message(),
        "mcp server remote is not logged in; run login remote"
    );
    assert!(
        setup.fixture.observed().requests().is_empty(),
        "a request was sent"
    );

    local_login(&pool, &spec).await?;
    let pinned = setup.spec(setup.settings().with_issuer("https://other.example.com")?)?;
    let refused = pool
        .lease(pinned, Arc::clone(&workspace.environment))
        .await
        .err()
        .ok_or("another server's login was used")?;
    assert!(
        refused
            .message()
            .contains("has a login made for another URL, issuer or client"),
        "{refused}"
    );
    Ok(())
}

#[tokio::test]
async fn hostile_scopes_never_reach_a_step_up_command() -> TestResult {
    for hostile in ["$(id)", "`id`", "a;id", "a'quote", "a\"quote", "a\x1b"] {
        assert!(McpLoginOptions::new().with_scope(hostile).is_err());
        let metadata_setup = setup(OAuthConfig {
            scopes_supported: vec![
                "mcp:tools".to_owned(),
                hostile.to_owned(),
                "offline_access".to_owned(),
            ],
            ..OAuthConfig::default()
        })
        .await?;
        let metadata_spec = metadata_setup.spec(metadata_setup.settings())?;
        let login = McpPool::new()
            .login(&metadata_spec, McpLoginOptions::new())
            .await?;
        assert_eq!(login.scope(), Some("mcp:tools offline_access"));
        drop(login);
        let setup = setup(OAuthConfig::default()).await?;
        let spec = setup.spec(setup.settings())?;
        let pool = McpPool::new();
        local_login(&pool, &spec).await?;
        let workspace = workspace()?;
        let registry = registry(&pool, spec, &workspace).await?;
        setup.gate.require_scope(Some(hostile));
        let refused = echo(&registry, &workspace).await?;
        assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
        assert!(
            !refused.joined_text().contains(hostile),
            "{}",
            refused.joined_text()
        );
        assert!(!refused.joined_text().contains("--scope"));
        assert_eq!(refused.joined_text().matches("run login remote").count(), 1);
        assert_eq!(setup.calls.calls(), Vec::<String>::new());
        pool.shutdown().await;
    }
    Ok(())
}
