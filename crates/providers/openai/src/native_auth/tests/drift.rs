use super::*;
use plakous_auth::CompatibilityDrift;

#[tokio::test]
async fn login_jwks_without_rs256_is_typed_before_account_or_storage() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let body = serde_json::json!({"keys":[{"kty":"EC","alg":"ES256","use":"sig","kid":"fixture-key","crv":"P-256","x":"fixture-x","y":"fixture-y"}]}).to_string();
    let server = spawn_sequence(
        endpoint,
        vec![response("200 OK", "application/json", body.as_bytes(), "")],
    );
    let token = id_token(TEST_KID, "fixture-subject", "fixture-account")?;
    let tokens = parse_strict_json(&token_body(
        &token,
        "fixture-access",
        "fixture-refresh",
        3600,
    )?)?;
    let failure = manager
        .validate_token_response(tokens, None, true, Instant::now() + Duration::from_secs(5))
        .await
        .err()
        .ok_or("unsupported signing algorithm admitted")?;
    assert!(matches!(
        failure,
        NativeCodexAuthError::CompatibilityDrift(CompatibilityDrift::SigningAlgorithm)
    ));
    assert!(failure.to_string().contains("install a newer build"));
    assert!(manager.store.load()?.is_none());
    assert_eq!(server.await??.len(), 1);
    Ok(())
}

#[tokio::test]
async fn mixed_jwks_ignores_other_algorithms_on_login_and_refresh() -> TestResult {
    for login in [true, false] {
        let endpoint = test_endpoint().await?;
        let (_directory, slot) = private_slot()?;
        let manager = manager_for_endpoint(&slot, &endpoint)?;
        let mut body: serde_json::Value =
            serde_json::from_slice(&jwks_body(TEST_KID, TEST_MODULUS)?)?;
        body["keys"].as_array_mut().ok_or("keys")?.insert(
            0,
            serde_json::json!({"kty":"EC","alg":"ES256","kid":"other"}),
        );
        let server = spawn_sequence(
            endpoint,
            vec![response(
                "200 OK",
                "application/json",
                body.to_string().as_bytes(),
                "",
            )],
        );
        assert_eq!(
            manager
                .jwk_for(TEST_KID, login, Instant::now() + Duration::from_secs(5))
                .await?
                .key_id(),
            TEST_KID
        );
        assert_eq!(server.await??.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn catalog_drift_refuses_only_the_changed_enrolled_row() -> TestResult {
    for (field, value) in [
        ("minimal_client_version", serde_json::json!("0.170.0")),
        ("tool_mode", serde_json::json!("future_mode")),
        ("visibility", serde_json::json!("future_visibility")),
        ("use_responses_lite", serde_json::json!("future_wire")),
        ("input_modalities", serde_json::json!(["image"])),
        ("shell_type", serde_json::json!("disabled")),
        ("supported_reasoning_levels", serde_json::json!([])),
    ] {
        let mut changed = catalog_model_value("gpt-6.1-sol");
        changed[field] = value;
        let (_dir, resolver, catalog) = resolver_for_catalog_values(
            "account-drift",
            vec![changed, catalog_model_value("gpt-5.6-luna")],
        )
        .await?;
        assert_eq!(catalog.entries().len(), 1, "{field}");
        assert_eq!(catalog.drifted_models().len(), 1, "{field}");
        let error = resolver
            .resolve("gpt-6.1-sol")
            .err()
            .ok_or("drifted model admitted")?;
        assert!(
            matches!(error, NativeCodexAuthError::CompatibilityDrift(_)),
            "{field}: {error}"
        );
        assert!(error.to_string().contains("install a newer build"));
        assert!(resolver.resolve("gpt-5.6-luna").is_ok());
    }
    Ok(())
}

#[tokio::test]
async fn catalog_drift_refuses_route_when_no_enrolled_row_is_admissible() -> TestResult {
    let mut changed = catalog_model_value("gpt-5.6-luna");
    changed["minimal_client_version"] = serde_json::json!("0.170.0");
    let failure = resolver_for_catalog_values("account-drift-only", vec![changed])
        .await
        .err()
        .ok_or("drifted route admitted")?;
    assert!(matches!(
        failure.downcast_ref::<NativeCodexAuthError>(),
        Some(NativeCodexAuthError::CompatibilityDrift(
            CompatibilityDrift::ClientVersion { .. }
        ))
    ));
    assert!(failure.to_string().contains("install a newer build"));
    Ok(())
}

#[tokio::test]
async fn drifted_rows_retain_provider_visibility() -> TestResult {
    for visibility in ["list", "hide", "none"] {
        let mut changed = catalog_model_value("gpt-6.1-sol");
        changed["minimal_client_version"] = serde_json::json!("99.0.0");
        changed["visibility"] = serde_json::json!(visibility);
        let (_dir, _resolver, catalog) = resolver_for_catalog_values(
            "account-visibility",
            vec![changed, catalog_model_value("gpt-5.6-luna")],
        )
        .await?;
        let expected = match visibility {
            "list" => CodexCatalogVisibility::List,
            "hide" => CodexCatalogVisibility::Hide,
            _ => CodexCatalogVisibility::None,
        };
        assert_eq!(catalog.drifted_visibility("gpt-6.1-sol"), Some(expected));
    }
    Ok(())
}

#[tokio::test]
async fn catalog_served_authority_and_unenrolled_changes_are_not_drift() -> TestResult {
    let mut valid = catalog_model_value("gpt-5.6-luna");
    valid["minimal_client_version"] =
        serde_json::json!(crate::OPENAI_CODEX_REFERENCE_CLIENT_VERSION);
    valid["context_window"] = serde_json::json!(999_000);
    valid["supported_reasoning_levels"] =
        serde_json::json!([{"effort":"high"},{"effort":"future"}]);
    valid["future_field"] = serde_json::json!({"new":"authority"});
    let mut unenrolled = catalog_model_value("unenrolled-future-model");
    unenrolled["minimal_client_version"] = serde_json::json!("99.0.0");
    let (_dir, resolver, catalog) =
        resolver_for_catalog_values("account-served", vec![valid, unenrolled]).await?;
    assert!(catalog.drifted_models().is_empty());
    assert!(resolver.resolve("gpt-5.6-luna").is_ok());
    Ok(())
}
