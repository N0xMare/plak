use super::*;

#[test]
fn scope_ingestion_rejects_shell_and_control_characters() {
    for hostile in [
        "$(id)", "`id`", "a;id", "a'quote", "a\"quote", "a\x1b", "a\nb", "a\tb", "a\u{85}",
    ] {
        assert!(!valid_scope_list(hostile));
        let quoted = hostile.replace('\\', "\\\\").replace('"', "\\\"");
        let challenge = format!("Bearer error=\"insufficient_scope\", scope=\"{quoted}\"");
        assert!(parse_bearer_challenge(&challenge).is_none());
        let body = serde_json::json!({
            "resource": "https://mcp.example.com/",
            "authorization_servers": ["https://auth.example.com"],
            "scopes_supported": ["files:read", hostile],
        })
        .to_string();
        let prm = parse_protected_resource_metadata(
            body.as_bytes(),
            "https://mcp.example.com/",
            "https://mcp.example.com/.well-known/oauth-protected-resource",
        )
        .unwrap();
        assert_eq!(prm.resource, "https://mcp.example.com/");
        assert_eq!(prm.scopes_supported, ["files:read"]);
        let body = serde_json::json!({
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/auth",
            "token_endpoint": "https://auth.example.com/token",
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["offline_access", hostile],
        })
        .to_string();
        let metadata =
            parse_authorization_server_metadata(body.as_bytes(), "https://auth.example.com")
                .unwrap();
        assert_eq!(metadata.scopes_supported, ["offline_access"]);
    }
    assert!(valid_scope_list("a._:/+-Z9 files:read"));
}

#[test]
fn bearer_challenges_are_found_among_others_and_quoted_values_unescape() {
    let challenge = parse_bearer_challenge(
        r#"Basic realm="x", Bearer error="insufficient_scope", scope="files:read files:write", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource/mcp""#,
    )
    .expect("bearer");
    assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
    assert_eq!(challenge.scope.as_deref(), Some("files:read files:write"));
    assert_eq!(
        challenge.resource_metadata.as_deref(),
        Some("https://mcp.example.com/.well-known/oauth-protected-resource/mcp")
    );
    assert_eq!(
        parse_bearer_challenge("bearer"),
        Some(BearerChallenge::default())
    );
    assert_eq!(
        parse_bearer_challenge(r#"Bearer realm="a\"b", error=invalid_token"#)
            .and_then(|challenge| challenge.error),
        Some(String::from("invalid_token"))
    );
    assert_eq!(parse_bearer_challenge("Basic realm=\"x\""), None);
    assert_eq!(
        parse_bearer_challenge(r#"Bearer scope="a", scope="b""#),
        None
    );
    assert_eq!(parse_bearer_challenge("Bearer scope=\"unterminated"), None);
}

#[test]
fn canonical_resource_uris_lowercase_and_drop_the_default_port_and_bare_slash() {
    for (input, canonical) in [
        ("https://MCP.Example.com:443/", "https://mcp.example.com"),
        ("https://mcp.example.com", "https://mcp.example.com"),
        ("https://mcp.example.com/mcp", "https://mcp.example.com/mcp"),
        (
            "https://mcp.example.com:8443/mcp/",
            "https://mcp.example.com:8443/mcp/",
        ),
        ("http://127.0.0.1:8080/mcp", "http://127.0.0.1:8080/mcp"),
    ] {
        assert_eq!(
            canonical_resource_uri(input).as_deref(),
            Ok(canonical),
            "{input}"
        );
    }
    for refused in [
        "https://mcp.example.com/mcp?tenant=a",
        "https://mcp.example.com/mcp#x",
        "https://user@mcp.example.com/",
        "ftp://mcp.example.com/",
        "/mcp",
    ] {
        assert!(canonical_resource_uri(refused).is_err(), "{refused}");
    }
}

#[test]
fn metadata_urls_follow_the_specification_order() {
    assert_eq!(
        protected_resource_metadata_urls("https://mcp.example.com/v1/mcp").unwrap(),
        [
            "https://mcp.example.com/.well-known/oauth-protected-resource/v1/mcp",
            "https://mcp.example.com/.well-known/oauth-protected-resource",
        ]
    );
    assert_eq!(
        protected_resource_metadata_urls("https://mcp.example.com/").unwrap(),
        ["https://mcp.example.com/.well-known/oauth-protected-resource"]
    );
    assert_eq!(
        authorization_server_metadata_urls("https://auth.example.com/tenant1").unwrap(),
        [
            "https://auth.example.com/.well-known/oauth-authorization-server/tenant1",
            "https://auth.example.com/.well-known/openid-configuration/tenant1",
            "https://auth.example.com/tenant1/.well-known/openid-configuration",
        ]
    );
    assert_eq!(
        authorization_server_metadata_urls("https://auth.example.com").unwrap(),
        [
            "https://auth.example.com/.well-known/oauth-authorization-server",
            "https://auth.example.com/.well-known/openid-configuration",
        ]
    );
}

#[test]
fn resource_metadata_covers_only_same_origin_path_segments() {
    let metadata = |resource: &str, endpoint: &str| {
        let published = Url::parse(resource).unwrap();
        let path = published
            .path()
            .strip_suffix('/')
            .unwrap_or(published.path());
        parse_protected_resource_metadata(
            serde_json::json!({
                "resource": resource,
                "authorization_servers": ["https://auth.example.com"],
            })
            .to_string()
            .as_bytes(),
            endpoint,
            &format!(
                "{}{}{}",
                published.origin().ascii_serialization(),
                PROTECTED_RESOURCE_WELL_KNOWN,
                path
            ),
        )
    };
    for resource in [
        "https://mcp.example.com",
        "https://mcp.example.com/",
        "https://MCP.example.com:443/v1",
        "https://mcp.example.com/v1/",
        "https://mcp.example.com/v1/mcp",
        "https://mcp.example.com/v1/mcp/",
    ] {
        let parsed = metadata(resource, "https://mcp.example.com/v1/mcp").unwrap();
        assert_eq!(parsed.resource, resource);
    }
    assert!(
        metadata(
            "https://mcp.example.com/mcp",
            "https://mcp.example.com/mcp/x"
        )
        .is_ok()
    );
    assert!(metadata("http://127.0.0.1:8080/", "http://127.0.0.1:8080/mcp").is_ok());
    assert!(metadata("http://[::1]:8080/", "http://[::1]:8080/mcp").is_ok());
    for (resource, endpoint) in [
        (
            "https://mcp.example.com/mcp",
            "https://mcp.example.com/mcp2",
        ),
        (
            "https://mcp.example.com/mcp/x",
            "https://mcp.example.com/mcp",
        ),
        (
            "https://mcp.example.com:8443/mcp",
            "https://mcp.example.com/mcp",
        ),
        (
            "https://other.example.com/mcp",
            "https://mcp.example.com/mcp",
        ),
        ("http://mcp.example.com/mcp", "https://mcp.example.com/mcp"),
        ("http://mcp.example.com/", "http://mcp.example.com/mcp"),
        (
            "https://mcp.example.com/mcp#x",
            "https://mcp.example.com/mcp",
        ),
        (
            "https://mcp.example.com/mcp//",
            "https://mcp.example.com/mcp",
        ),
    ] {
        assert_eq!(
            metadata(resource, endpoint).err(),
            Some(OAuthWorkflowError::ResourceMismatch),
            "{resource} {endpoint}"
        );
    }
    assert_eq!(
        parse_protected_resource_metadata(
            br#"{"resource":"https://mcp.example.com/"}"#,
            "https://mcp.example.com/mcp",
            "https://mcp.example.com/.well-known/oauth-protected-resource"
        )
        .err(),
        Some(OAuthWorkflowError::MissingMetadata)
    );
}

#[test]
fn resource_coverage_is_location_bound_and_never_repairs_paths() {
    let parse = |resource: &str, endpoint: &str, location: &str| {
        parse_protected_resource_metadata(
            serde_json::json!({"resource": resource, "authorization_servers": ["https://auth.example"]}).to_string().as_bytes(),
            endpoint, location,
        )
    };
    for location in [
        "https://shared.example/tenantA/metadata",
        "https://shared.example/.well-known/oauth-protected-resource/tenantA/mcp",
    ] {
        assert!(
            parse(
                "https://shared.example",
                "https://shared.example/tenantA/mcp",
                location
            )
            .is_err()
        );
    }
    assert!(
        parse(
            "https://shared.example",
            "https://shared.example/tenantA/mcp",
            "https://shared.example/.well-known/oauth-protected-resource"
        )
        .is_ok()
    );
    assert!(
        parse(
            "https://shared.example",
            "https://shared.example/mcp",
            "https://SHARED.example:443/.well-known/oauth-protected-resource"
        )
        .is_ok()
    );
    for resource in [
        "https://shared.example/a/../mcp",
        "https://shared.example/./mcp",
        "https://shared.example/%2e/mcp",
        "https://shared.example/a/%2E%2e/mcp",
        "https://shared.example/%2F/mcp",
        "https://shared.example/a\\mcp",
        " https://shared.example/mcp",
        "\u{1}https://shared.example/mcp",
        "https://shared.example/mcp\u{7f}",
        "https://shared.example/mcp\t",
        "https://shared.example/m cp",
        "https://shared.example/mcp?x=y",
        "https://user@shared.example/mcp",
        "https://shared.example/mcp#x",
        "https://b\u{fc}cher.example/mcp",
        "https://%73hared.example/mcp",
        "http://127.1/mcp",
    ] {
        assert!(
            parse(
                resource,
                "https://shared.example/mcp",
                "https://shared.example/metadata"
            )
            .is_err(),
            "{resource}"
        );
    }
    for (raw, endpoint) in [
        (
            "https://b\u{fc}cher.example/mcp",
            "https://xn--bcher-kva.example/mcp",
        ),
        ("http://127.1/mcp", "http://127.0.0.1/mcp"),
        (
            "https://shared.example/caf\u{e9}",
            "https://shared.example/caf%C3%A9",
        ),
    ] {
        assert!(
            parse(raw, endpoint, "https://shared.example/custom").is_err(),
            "{raw}"
        );
    }
    assert!(
        parse(
            "HTTPS://SHARED.example:443/mcp/",
            "https://shared.example/mcp",
            "https://shared.example/custom"
        )
        .is_ok()
    );
    assert!(
        parse(
            "https://xn--bcher-kva.example/mcp",
            "https://xn--bcher-kva.example/mcp",
            "https://xn--bcher-kva.example/custom"
        )
        .is_ok()
    );
}

fn server_metadata(extra: serde_json::Value) -> Vec<u8> {
    let mut document = serde_json::json!({
        "issuer": "https://auth.example.com",
        "authorization_endpoint": "https://auth.example.com/authorize",
        "token_endpoint": "https://auth.example.com/token",
        "code_challenge_methods_supported": ["plain", "S256"],
    });
    if let (Some(document), serde_json::Value::Object(extra)) = (document.as_object_mut(), extra) {
        document.extend(extra);
    }
    document.to_string().into_bytes()
}

#[test]
fn server_metadata_needs_the_exact_issuer_and_s256() {
    let parsed = parse_authorization_server_metadata(
        &server_metadata(
            serde_json::json!({"authorization_response_iss_parameter_supported": true}),
        ),
        "https://auth.example.com",
    )
    .unwrap();
    assert!(parsed.authorization_response_iss_parameter_supported);
    assert_eq!(parsed.registration_endpoint, None);
    assert_eq!(
        parse_authorization_server_metadata(
            &server_metadata(serde_json::json!({})),
            "https://auth.example.com/"
        )
        .err(),
        Some(OAuthWorkflowError::IssuerMismatch)
    );
    for methods in [serde_json::json!(null), serde_json::json!(["plain"])] {
        let mut body: serde_json::Value =
            serde_json::from_slice(&server_metadata(serde_json::json!({}))).unwrap();
        body["code_challenge_methods_supported"] = methods;
        assert_eq!(
            parse_authorization_server_metadata(
                body.to_string().as_bytes(),
                "https://auth.example.com"
            )
            .err(),
            Some(OAuthWorkflowError::PkceUnsupported)
        );
    }
}

#[test]
fn authorization_urls_keep_the_endpoint_query_and_carry_the_resource() {
    let url = build_resource_authorization_url(ResourceAuthorizeRequest {
        authorization_endpoint: "https://auth.example.com/authorize?tenant=a",
        client_id: "client-1",
        redirect_uri: "http://127.0.0.1:4321/callback",
        scope: Some("files:read offline_access"),
        code_challenge: "challenge",
        state: "state-1",
        resource: "https://mcp.example.com/mcp",
    })
    .unwrap();
    let pairs = Url::parse(&url)
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let keys = pairs
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "tenant",
            "response_type",
            "client_id",
            "redirect_uri",
            "scope",
            "code_challenge",
            "code_challenge_method",
            "state",
            "resource"
        ]
    );
    assert_eq!(
        pairs.last().map(|(_, value)| value.as_str()),
        Some("https://mcp.example.com/mcp")
    );
}

#[test]
fn the_issuer_parameter_is_split_off_and_checked() {
    let (rest, issuer) = take_issuer_parameter(
        "http://127.0.0.1:4321/callback?code=c%2B1&state=s&iss=https%3A%2F%2Fauth.example.com",
    )
    .unwrap();
    assert_eq!(
        rest.as_str(),
        "http://127.0.0.1:4321/callback?code=c%2B1&state=s"
    );
    assert_eq!(
        issuer.as_deref().map(String::as_str),
        Some("https://auth.example.com")
    );
    assert!(take_issuer_parameter("code=c&iss=a&iss=a").is_err());
    assert!(
        check_callback_issuer(
            Some("https://auth.example.com"),
            "https://auth.example.com",
            true
        )
        .is_ok()
    );
    assert!(check_callback_issuer(None, "https://auth.example.com", false).is_ok());
    assert_eq!(
        check_callback_issuer(None, "https://auth.example.com", true),
        Err(OAuthWorkflowError::IssuerMismatch)
    );
    assert_eq!(
        check_callback_issuer(
            Some("https://evil.example.com"),
            "https://auth.example.com",
            false
        ),
        Err(OAuthWorkflowError::IssuerMismatch)
    );
}

#[test]
fn token_registration_and_device_responses_parse_strictly() {
    let token = parse_resource_token_response(
        br#"{"access_token":"at-1","token_type":"bearer","expires_in":3600,"refresh_token":"rt-1","scope":"a b"}"#,
    )
    .unwrap();
    assert_eq!(token.access_token.expose_secret(), "at-1");
    assert_eq!(token.expires_in, Some(3600));
    assert_eq!(
        token
            .refresh_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some("rt-1")
    );
    assert!(parse_resource_token_response(br#"{"access_token":"at","token_type":"mac"}"#).is_err());
    assert!(!format!("{token:?}").contains("at-1"));
    assert_eq!(
        oauth_error_code(br#"{"error":"authorization_pending"}"#).as_deref(),
        Some("authorization_pending")
    );
    let registration =
        parse_dynamic_registration(br#"{"client_id":"dyn-1","client_secret":"s"}"#).unwrap();
    assert_eq!(registration.client_id, "dyn-1");
    assert!(registration.client_secret.is_some());
    let request: serde_json::Value = serde_json::from_slice(&dynamic_registration_request(
        "Plak",
        "http://127.0.0.1:1/callback",
        false,
    ))
    .unwrap();
    assert_eq!(
        request["redirect_uris"],
        serde_json::json!(["http://127.0.0.1:1/callback"])
    );
    assert_eq!(request["token_endpoint_auth_method"], "none");
    let device = parse_device_authorization(
        br#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://auth.example.com/device","expires_in":600,"interval":5}"#,
    )
    .unwrap();
    assert_eq!(device.user_code, "ABCD-EFGH");
    assert_eq!(device.interval, Some(5));
}

#[test]
fn registration_requests_the_device_grant_only_when_the_caller_admits_it() {
    for device in [false, true] {
        let request: serde_json::Value = serde_json::from_slice(&dynamic_registration_request(
            "Plak",
            "http://127.0.0.1:1/callback",
            device,
        ))
        .unwrap();
        let mut expected = vec!["authorization_code", "refresh_token"];
        if device {
            expected.push("urn:ietf:params:oauth:grant-type:device_code");
        }
        assert_eq!(request["grant_types"], serde_json::json!(expected));
    }
}

#[test]
fn scope_unions_keep_first_seen_order_without_repeats() {
    assert_eq!(
        union_scopes([Some("a b"), None, Some("b  c"), Some("")]).as_deref(),
        Some("a b c")
    );
    assert_eq!(union_scopes([None, Some(" ")]), None);
}
