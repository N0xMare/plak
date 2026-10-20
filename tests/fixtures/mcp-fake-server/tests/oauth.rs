//! MCP authorization against the scripted authorization server: discovery,
//! registration, the local, paste-back and device logins, and logout.

use std::sync::Arc;
use std::time::Duration;

use plakous_mcp::{
    McpHttpEndpoint, McpLocalRetirement, McpLoginOptions, McpLogoutOutcome, McpOAuthSettings,
    McpOAuthStatus, McpPool, McpRemoteLogin, McpServerSpec, SensitiveBytes,
};
use plakous_mcp_fake_server::http::{HttpFixture, HttpGate, serve_http};
use plakous_mcp_fake_server::oauth::{
    CallbackIssuer, OAuthConfig, OAuthGate, ResourceMetadataAt, ServerMetadataAt, browse,
};
use plakous_mcp_fake_server::{FakeServer, Style};
use plakous_tools::McpServerName;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Setup {
    fixture: HttpFixture,
    gate: Arc<OAuthGate>,
    home: tempfile::TempDir,
}

async fn setup(config: OAuthConfig) -> TestResult<Setup> {
    let gate = Arc::new(OAuthGate::new(config));
    let fixture = serve_http(
        FakeServer::new(Style::Discover, Arc::new(|| {})),
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
        home,
    })
}

impl Setup {
    fn settings(&self) -> McpOAuthSettings {
        McpOAuthSettings::new(self.home.path().join("remote").join("credential.json"))
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

    // A resource metadata naming a path-segment prefix of the server is
    // accepted, and its value is the resource indicator.
    let prefix = setup(OAuthConfig {
        resource: Some("/".to_owned()),
        ..OAuthConfig::default()
    })
    .await?;
    let spec = prefix.spec(prefix.settings())?;
    let pool = McpPool::new();
    assert!(logged_in(&local_login(&pool, &spec).await?));
    let indicator = prefix.fixture.url().trim_end_matches("/mcp").to_owned();
    assert_eq!(
        prefix.gate.requests_to("/token")[0].field("resource"),
        Some(indicator.as_str())
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
