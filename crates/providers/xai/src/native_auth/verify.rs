//! Provider-specific signed identity, signing key, and token validation.

use super::{
    AccountBinding, Algorithm, AlgorithmParameters, BTreeMap, BTreeSet,
    CredentialRefreshFailureReason, DEFAULT_TOKEN_LIFETIME_SECONDS, DecodingKey, Deserialize,
    EllipticCurve, FetchedJwks, Instant, JWKS_TTL, Jwk, JwkSet, KeyAlgorithm, KeyOperations,
    LoginHttpStage, MAX_AUTH_BODY_BYTES, MAX_JWKS_KEYS, MAX_STRING_BYTES, ManagerCore,
    PublicKeyUse, STORED_TOKENS_VERSION, StoredTokens, TOKEN_CLOCK_LEEWAY_SECONDS, TokenResponse,
    Validation, XAI_ISSUER, XAI_OAUTH_SCOPE, XAI_PUBLIC_CLIENT_ID, XaiNativeAuthError, decode,
    decode_header, decode_json, generate_selection_epoch, login_account_verification_or,
    login_signing_keys_or, login_token_exchange_or, system_unix_seconds, unsigned_access_token_exp,
    valid_login_selection_epoch, valid_token_component,
};
use zeroize::Zeroize as _;

impl ManagerCore {
    pub(super) async fn validate_tokens(
        &self,
        mut tokens: TokenResponse,
        nonce: Option<&str>,
        expected_subject: Option<&AccountBinding>,
        old_refresh: Option<&str>,
        retained_selection_epoch: Option<&str>,
        flow_deadline: Option<Instant>,
    ) -> Result<StoredTokens, XaiNativeAuthError> {
        self.ensure_login_validation_active(flow_deadline)?;
        let login = flow_deadline.is_some();
        tokens
            .validate()
            .map_err(|error| login_token_exchange_or(login, error))?;
        let now = system_unix_seconds()?;
        // Login (browser/device) requires a signed `id_token` so account binding
        // is not solely UserInfo after a successful code exchange. Refresh remains
        // more lenient: a rotating refresh response may omit `id_token`, and a
        // present token is still fully verified when supplied.
        let id_subject = if login {
            let id_token = tokens.id_token.as_deref().ok_or_else(|| {
                login_token_exchange_or(
                    true,
                    XaiNativeAuthError::InvalidTokenResponse(
                        CredentialRefreshFailureReason::TokenShape,
                    ),
                )
            })?;
            Some(
                self.validate_id_token(id_token, nonce, now, flow_deadline)
                    .await?,
            )
        } else if let Some(id_token) = tokens.id_token.as_deref() {
            Some(self.validate_id_token(id_token, nonce, now, None).await?)
        } else {
            None
        };
        self.ensure_login_validation_active(flow_deadline)?;
        if let (Some(expected), Some(actual)) = (expected_subject, id_subject.as_ref())
            && expected != actual
        {
            return Err(login_account_verification_or(
                login,
                XaiNativeAuthError::AccountChanged,
            ));
        }
        let user_subject = if let Some(expected) = expected_subject {
            // The rotating refresh credential is already fenced to this
            // selected account. A supplied signed ID subject was compared
            // above; do not make UserInfo an additional refresh dependency.
            expected.clone()
        } else {
            let user_subject = self.userinfo(&tokens.access_token, login).await?;
            self.ensure_login_validation_active(flow_deadline)?;
            if id_subject
                .as_ref()
                .is_some_and(|subject| subject != &user_subject)
            {
                return Err(login_account_verification_or(
                    login,
                    XaiNativeAuthError::AccountChanged,
                ));
            }
            user_subject
        };
        let refresh = if tokens.refresh_token.is_empty() {
            old_refresh.map(ToOwned::to_owned).ok_or_else(|| {
                login_token_exchange_or(
                    login,
                    XaiNativeAuthError::InvalidTokenResponse(
                        CredentialRefreshFailureReason::MissingRefreshToken,
                    ),
                )
            })?
        } else {
            std::mem::take(&mut tokens.refresh_token)
        };
        let reported_expires_at = now
            .checked_add(tokens.expires_in.unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS))
            .ok_or_else(|| {
                login_token_exchange_or(
                    login,
                    XaiNativeAuthError::InvalidTokenResponse(
                        CredentialRefreshFailureReason::ExpiresIn,
                    ),
                )
            })?;
        // A decoded access-token expiry is only a scheduling hint. Bound the
        // provider default by it so an omitted `expires_in` cannot keep an
        // already-expired JWT apparently ready.
        let expires_at = unsigned_access_token_exp(&tokens.access_token, MAX_STRING_BYTES)
            .map_or(reported_expires_at, |expires_at| {
                expires_at.min(reported_expires_at)
            });
        // Scheduling uses ACCESS_REFRESH_LEAD_SECONDS before expiry; reject
        // only tokens already expired or within TOKEN_CLOCK_LEEWAY_SECONDS of expiry.
        // Using the lead here would reject every refresh whose remaining TTL is
        // inside the refresh window — the normal case after a successful exchange.
        if expires_at <= now.saturating_add(TOKEN_CLOCK_LEEWAY_SECONDS) {
            return Err(login_token_exchange_or(
                login,
                XaiNativeAuthError::InvalidTokenResponse(
                    CredentialRefreshFailureReason::AccessTokenExpired,
                ),
            ));
        }
        Ok(StoredTokens {
            version: STORED_TOKENS_VERSION,
            issuer: XAI_ISSUER.to_owned(),
            client_id: XAI_PUBLIC_CLIENT_ID.to_owned(),
            subject: user_subject.as_str().to_owned(),
            scope: XAI_OAUTH_SCOPE.to_owned(),
            access_token: std::mem::take(&mut tokens.access_token),
            refresh_token: refresh,
            expires_at,
            selection_epoch: match retained_selection_epoch {
                Some(value) if valid_login_selection_epoch(value) => value.to_owned(),
                Some(_) => return Err(XaiNativeAuthError::InvalidStoredCredential),
                None => generate_selection_epoch()?,
            },
        })
    }

    async fn validate_id_token(
        &self,
        token: &str,
        nonce: Option<&str>,
        now: u64,
        flow_deadline: Option<Instant>,
    ) -> Result<AccountBinding, XaiNativeAuthError> {
        let login = flow_deadline.is_some();
        let invalid = |reason| {
            login_account_verification_or(login, XaiNativeAuthError::InvalidIdToken(reason))
        };
        let header = decode_header(token)
            .map_err(|_| invalid(CredentialRefreshFailureReason::IdTokenHeader))?;
        if header.alg != Algorithm::ES256
            || header.typ.as_deref().is_some_and(|value| value != "JWT")
            || header.jku.is_some()
            || header.jwk.is_some()
            || header.x5u.is_some()
            || header.x5c.is_some()
            || header.crit.is_some()
            || !header.extras.inner().is_empty()
        {
            return Err(invalid(CredentialRefreshFailureReason::IdTokenHeader));
        }
        let kid = header
            .kid
            .ok_or_else(|| invalid(CredentialRefreshFailureReason::IdTokenHeader))?;
        if !valid_token_component(&kid, 128) {
            return Err(invalid(CredentialRefreshFailureReason::IdTokenHeader));
        }
        let jwk = self.jwk_for(&kid, flow_deadline).await?;
        validate_jwk_metadata(&jwk, &kid).map_err(|error| login_signing_keys_or(login, error))?;
        let key = DecodingKey::from_jwk(&jwk)
            .map_err(|_| login_signing_keys_or(login, XaiNativeAuthError::InvalidJwks))?;
        let mut validation = Validation::new(Algorithm::ES256);
        validation.set_issuer(&[XAI_ISSUER]);
        validation.set_audience(&[XAI_PUBLIC_CLIENT_ID]);
        validation.set_required_spec_claims(&["iss", "aud", "sub", "exp"]);
        validation.leeway = TOKEN_CLOCK_LEEWAY_SECONDS;
        validation.reject_tokens_expiring_in_less_than = 0;
        let data = decode::<IdClaims>(token, &key, &validation)
            .map_err(|error| invalid(jwt_failure_reason(&error)))?;
        let mut claims = data.claims;
        let leeway_now = now.saturating_add(TOKEN_CLOCK_LEEWAY_SECONDS);
        let failed = if claims.iss != XAI_ISSUER {
            Some(CredentialRefreshFailureReason::IdTokenIss)
        } else if !claims.aud.is_exact(XAI_PUBLIC_CLIENT_ID) {
            Some(CredentialRefreshFailureReason::IdTokenAud)
        } else if claims
            .azp
            .as_deref()
            .is_some_and(|authorized_party| authorized_party != XAI_PUBLIC_CLIENT_ID)
        {
            Some(CredentialRefreshFailureReason::IdTokenAzp)
        } else if claims.iat > leeway_now {
            Some(CredentialRefreshFailureReason::IdTokenIat)
        } else if claims.nbf.is_some_and(|value| value > leeway_now) {
            Some(CredentialRefreshFailureReason::IdTokenNbf)
        } else if !valid_token_component(&claims.sub, 256) {
            Some(CredentialRefreshFailureReason::IdTokenSub)
        } else if nonce.is_some_and(|expected| claims.nonce.as_deref() != Some(expected)) {
            Some(CredentialRefreshFailureReason::IdTokenClaims)
        } else {
            None
        };
        if let Some(reason) = failed {
            return Err(invalid(reason));
        }
        AccountBinding::new(std::mem::take(&mut claims.sub))
            .map_err(|error| login_account_verification_or(login, XaiNativeAuthError::from(error)))
    }

    /// A login bounds the single-flight wait and fetch by its flow deadline,
    /// failing with the same `LoginExpired` the enclosing flow would.
    pub(super) async fn jwk_for(
        &self,
        kid: &str,
        flow_deadline: Option<Instant>,
    ) -> Result<Jwk, XaiNativeAuthError> {
        let login = flow_deadline.is_some();
        // Boxed so the single-flight wait does not inflate every login future.
        Box::pin(self.jwks.resolve(
            kid,
            flow_deadline,
            || XaiNativeAuthError::LoginExpired,
            || login_signing_keys_or(login, XaiNativeAuthError::UnknownSigningKey),
            || self.fetch_jwks_snapshot(login),
        ))
        .await
    }

    async fn fetch_jwks_snapshot(
        &self,
        login: bool,
    ) -> Result<FetchedJwks<Jwk>, XaiNativeAuthError> {
        let response = if login {
            self.get_login_json(self.endpoints.jwks(), None, LoginHttpStage::SigningKeys)
                .await?
        } else {
            self.get(self.endpoints.jwks(), None, None, false).await?
        };
        let set: JwkSet = decode_json(&response, MAX_AUTH_BODY_BYTES)
            .map_err(|error| login_signing_keys_or(login, error))?;
        validate_jwks(&set).map_err(|error| login_signing_keys_or(login, error))?;
        let mut keys = BTreeMap::new();
        for key in set.keys {
            let kid = key
                .common
                .key_id
                .clone()
                .ok_or_else(|| login_signing_keys_or(login, XaiNativeAuthError::InvalidJwks))?;
            if keys.insert(kid, key).is_some() {
                return Err(login_signing_keys_or(
                    login,
                    XaiNativeAuthError::InvalidJwks,
                ));
            }
        }
        Ok(FetchedJwks {
            keys,
            expires_at: Instant::now() + JWKS_TTL,
        })
    }

    async fn userinfo(
        &self,
        access_token: &str,
        login: bool,
    ) -> Result<AccountBinding, XaiNativeAuthError> {
        let response = if login {
            self.get_login_json(
                self.endpoints.userinfo(),
                Some(access_token),
                LoginHttpStage::AccountVerification,
            )
            .await?
        } else {
            self.get(self.endpoints.userinfo(), Some(access_token), None, false)
                .await?
        };
        let mut info: UserInfo = decode_json(&response, MAX_AUTH_BODY_BYTES)
            .map_err(|error| login_account_verification_or(login, error))?;
        AccountBinding::new(std::mem::take(&mut info.sub))
            .map_err(|error| login_account_verification_or(login, XaiNativeAuthError::from(error)))
    }
}

#[derive(Deserialize)]
struct IdClaims {
    iss: String,
    aud: Audience,
    azp: Option<String>,
    sub: String,
    #[expect(dead_code, reason = "validated by jsonwebtoken decode leeway")]
    exp: u64,
    iat: u64,
    nbf: Option<u64>,
    nonce: Option<String>,
}

impl Drop for IdClaims {
    fn drop(&mut self) {
        self.iss.zeroize();
        self.azp.zeroize();
        self.sub.zeroize();
        self.nonce.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn is_exact(&self, expected: &str) -> bool {
        match self {
            Self::One(value) => value == expected,
            Self::Many(values) => values.len() == 1 && values[0] == expected,
        }
    }
}

impl Drop for Audience {
    fn drop(&mut self) {
        match self {
            Self::One(value) => value.zeroize(),
            Self::Many(values) => values.zeroize(),
        }
    }
}

#[derive(Deserialize)]
struct UserInfo {
    sub: String,
}

impl Drop for UserInfo {
    fn drop(&mut self) {
        self.sub.zeroize();
    }
}

/// Names the `jsonwebtoken` check that rejected an ID token (content-free).
fn jwt_failure_reason(error: &jsonwebtoken::errors::Error) -> CredentialRefreshFailureReason {
    use jsonwebtoken::errors::ErrorKind as K;
    match error.kind() {
        K::InvalidSignature => CredentialRefreshFailureReason::IdTokenSignature,
        K::ExpiredSignature => CredentialRefreshFailureReason::IdTokenExp,
        K::ImmatureSignature => CredentialRefreshFailureReason::IdTokenNbf,
        K::InvalidIssuer => CredentialRefreshFailureReason::IdTokenIss,
        K::InvalidAudience => CredentialRefreshFailureReason::IdTokenAud,
        K::InvalidSubject => CredentialRefreshFailureReason::IdTokenSub,
        K::MissingRequiredClaim(claim) if claim == "sub" => {
            CredentialRefreshFailureReason::IdTokenSub
        }
        K::MissingRequiredClaim(claim) if claim == "exp" => {
            CredentialRefreshFailureReason::IdTokenExp
        }
        K::InvalidAlgorithm => CredentialRefreshFailureReason::IdTokenHeader,
        K::InvalidToken | K::Base64(_) | K::Utf8(_) | K::Json(_) => {
            CredentialRefreshFailureReason::TokenShape
        }
        _ => CredentialRefreshFailureReason::IdTokenClaims,
    }
}

fn validate_jwks(set: &JwkSet) -> Result<(), XaiNativeAuthError> {
    if set.keys.is_empty() || set.keys.len() > MAX_JWKS_KEYS {
        return Err(XaiNativeAuthError::InvalidJwks);
    }
    let mut kids = BTreeSet::new();
    for key in &set.keys {
        let kid = key
            .common
            .key_id
            .as_deref()
            .ok_or(XaiNativeAuthError::InvalidJwks)?;
        if !valid_token_component(kid, 128) || !kids.insert(kid) {
            return Err(XaiNativeAuthError::InvalidJwks);
        }
        validate_jwk_metadata(key, kid)?;
    }
    Ok(())
}

fn validate_jwk_metadata(key: &Jwk, kid: &str) -> Result<(), XaiNativeAuthError> {
    if key.common.key_id.as_deref() != Some(kid)
        || key.common.key_algorithm != Some(KeyAlgorithm::ES256)
        || key.common.public_key_use.as_ref() != Some(&PublicKeyUse::Signature)
        || key
            .common
            .key_operations
            .as_ref()
            .is_some_and(|ops| ops.as_slice() != [KeyOperations::Verify])
        || !matches!(&key.algorithm, AlgorithmParameters::EllipticCurve(parameters) if parameters.curve == EllipticCurve::P256 && parameters.x.len() == 43 && parameters.y.len() == 43)
    {
        return Err(XaiNativeAuthError::InvalidJwks);
    }
    Ok(())
}
