use super::*;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::KeyPair as _;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

// Public, non-production fixture also used by the xAI signed-identity tests.
const PRIVATE_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgWTFfCGljY6aw3HrtkHmPRiazukxPLb6ilpRAewjW8nihRANCAATDskChT-Altkm9X7MI69T3IUmrQU0L950IxEzvw_x5BMEINRMrXLBJhqzO9Bm-d6JbqA21YQmd1Kt4RzLJR1W-";

fn signed_token() -> TestResult<(String, DecodingKey)> {
    let rng = ring::rand::SystemRandom::new();
    let pair = ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &URL_SAFE_NO_PAD.decode(PRIVATE_KEY)?,
        &rng,
    )
    .map_err(|_| "invalid fixture key")?;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(br#"{"sub":"fixture","exp":4102444800}"#);
    let message = format!("{header}.{claims}");
    let signature = pair
        .sign(&rng, message.as_bytes())
        .map_err(|_| "sign failed")?;
    Ok((
        format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref())),
        DecodingKey::from_ec_der(pair.public_key().as_ref()),
    ))
}

#[test]
fn verification_accepts_only_the_matching_key_and_untampered_message() -> TestResult {
    let (token, key) = signed_token()?;
    let validation = jsonwebtoken::Validation::new(Algorithm::ES256);
    let decoded =
        crate::oauth_orchestrator::jwt::decode::<serde_json::Value>(&token, &key, &validation)?;
    assert_eq!(decoded.claims["sub"], "fixture");
    let mut parts: Vec<_> = token.split('.').map(str::to_owned).collect();
    parts[1] = URL_SAFE_NO_PAD.encode(br#"{"sub":"changed","exp":4102444800}"#);
    let error = crate::oauth_orchestrator::jwt::decode::<serde_json::Value>(
        &parts.join("."),
        &key,
        &validation,
    )
    .err()
    .ok_or("tampered claims were accepted")?;
    assert_eq!(error.kind(), &ErrorKind::InvalidSignature);
    let rng = ring::rand::SystemRandom::new();
    let other_key = ring::signature::EcdsaKeyPair::generate_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &rng,
    )
    .map_err(|_| "second key generation failed")?;
    let other_pair = ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        other_key.as_ref(),
        &rng,
    )
    .map_err(|_| "second key parsing failed")?;
    let error = crate::oauth_orchestrator::jwt::decode::<serde_json::Value>(
        &token,
        &DecodingKey::from_ec_der(other_pair.public_key().as_ref()),
        &validation,
    )
    .err()
    .ok_or("different key was accepted")?;
    assert_eq!(error.kind(), &ErrorKind::InvalidSignature);
    Ok(())
}

#[test]
fn unsupported_algorithms_key_confusion_and_signing_refuse() {
    for algorithm in [
        Algorithm::HS256,
        Algorithm::ES384,
        Algorithm::RS384,
        Algorithm::EdDSA,
    ] {
        assert!(verifier(algorithm, &DecodingKey::from_secret(b"fixture")).is_err());
    }
    assert!(verifier(Algorithm::RS256, &DecodingKey::from_secret(b"fixture")).is_err());
    assert!(
        verifier(
            Algorithm::ES256,
            &DecodingKey::from_rsa_raw_components(&[1], &[3])
        )
        .is_err()
    );
    assert!(
        (PROVIDER.signer_factory)(
            &Algorithm::ES256,
            &jsonwebtoken::EncodingKey::from_secret(b"fixture")
        )
        .is_err()
    );
    let validation = jsonwebtoken::Validation::new(Algorithm::RS256);
    for algorithm in ["none", "HS256", "ES384"] {
        let token = format!(
            "{}.e30.AA",
            URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"{algorithm}"}}"#))
        );
        assert!(
            crate::oauth_orchestrator::jwt::decode::<serde_json::Value>(
                &token,
                &DecodingKey::from_rsa_raw_components(&[1], &[3]),
                &validation,
            )
            .is_err()
        );
    }
}

#[test]
fn first_decode_in_a_fresh_process_installs_without_crate_backends() -> TestResult {
    for test in ["fresh_process_decode", "fresh_process_embedder_provider"] {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                &format!("jwt_crypto::tests::{test}"),
                "--ignored",
            ])
            .status()?;
        assert!(status.success());
    }
    Ok(())
}

#[test]
#[ignore = "run by the fresh-process installation contract"]
fn fresh_process_decode() -> TestResult {
    verification_accepts_only_the_matching_key_and_untampered_message()
}

#[test]
#[ignore = "run by the fresh-process installation contract"]
fn fresh_process_embedder_provider() -> TestResult {
    static EMBEDDER: CryptoProvider = CryptoProvider {
        signer_factory: |_, _| Err(ErrorKind::InvalidAlgorithm.into()),
        verifier_factory: |_, _| Err(ErrorKind::InvalidKeyFormat.into()),
        key_utils: KEY_UTILS,
    };
    assert!(EMBEDDER.install_default().is_ok());
    let (token, key) = signed_token()?;
    let error = crate::oauth_orchestrator::jwt::decode::<serde_json::Value>(
        &token,
        &key,
        &jsonwebtoken::Validation::new(Algorithm::ES256),
    )
    .err()
    .ok_or("embedder verifier was replaced")?;
    assert_eq!(error.kind(), &ErrorKind::InvalidKeyFormat);
    Ok(())
}
