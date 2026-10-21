//! Provider-specific signed identity, signing key, and token validation.

use super::{
    ACCESS_REFRESH_SKEW, AUTH_REQUEST_CAP, AccountBinding, Algorithm, BTreeMap, BTreeSet,
    CodexSubscriptionCredentialManager, DEFAULT_ACCESS_TOKEN_LIFETIME, DecodingKey, Deserialize,
    Duration, FetchedJwks, Instant, JWKS_MAX_AGE, JWKS_PATH, MAX_ID_TOKEN_LIFETIME, MAX_JWKS_KEYS,
    MAX_TOKEN_BYTES, Method, NativeCodexAuthError, OAuthLoginTokenResponse,
    OAuthRefreshTokenResponse, OPENAI_AUTH_ORIGIN, OPENAI_CODEX_NATIVE_CLIENT_ID,
    OPENAI_CODEX_OAUTH_SCOPES, StatusCode, StoredCredential, USER_INFO_PATH, Validation,
    base_headers, bearer_headers, capped_login_deadline, constant_time_value_eq, decode,
    decode_header, execute_bounded, generate_selection_epoch, login_account_verification_or,
    login_signing_keys_or, login_token_response_or, parse_cache_control_max_age, parse_strict_json,
    remaining_login_time, require_json_media_type, require_status, scheduled_access_expiry,
    validate_identifier,
};
use base64::Engine as _;
use plakous_auth::oauth_orchestrator::CredentialRefreshFailureReason;
use zeroize::Zeroize as _;

#[derive(Clone, Deserialize)]
pub(super) struct RsaJwk {
    kty: String,
    #[serde(rename = "use")]
    usage: String,
    alg: String,
    kid: String,
    n: String,
    e: String,
}

#[cfg(test)]
impl RsaJwk {
    pub(super) fn key_id(&self) -> &str {
        &self.kid
    }

    pub(super) fn modulus(&self) -> &str {
        &self.n
    }

    pub(super) fn exponent(&self) -> &str {
        &self.e
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AudienceClaim {
    One(String),
    Many(Vec<String>),
}

impl Drop for AudienceClaim {
    fn drop(&mut self) {
        match self {
            Self::One(value) => value.zeroize(),
            Self::Many(values) => values.zeroize(),
        }
    }
}

#[derive(Deserialize)]
struct IdTokenClaims {
    iss: String,
    aud: AudienceClaim,
    #[serde(default)]
    azp: Option<String>,
    sub: String,
    exp: u64,
    iat: u64,
    #[serde(default)]
    nbf: Option<u64>,
    #[serde(rename = "https://api.openai.com/auth")]
    openai_auth: OpenAiIdentityClaims,
}

impl Drop for IdTokenClaims {
    fn drop(&mut self) {
        self.iss.zeroize();
        self.azp.zeroize();
        self.sub.zeroize();
    }
}

#[derive(Deserialize)]
struct OpenAiIdentityClaims {
    chatgpt_account_id: String,
}

impl Drop for OpenAiIdentityClaims {
    fn drop(&mut self) {
        self.chatgpt_account_id.zeroize();
    }
}

#[derive(Deserialize)]
struct UserInfoResponse {
    sub: String,
}

impl Drop for UserInfoResponse {
    fn drop(&mut self) {
        self.sub.zeroize();
    }
}

#[derive(Deserialize)]
struct JwksDocument {
    keys: Vec<RsaJwk>,
}

impl CodexSubscriptionCredentialManager {
    pub(super) async fn validate_token_response(
        &self,
        mut tokens: OAuthLoginTokenResponse,
        expected_identity: Option<(&str, &str)>,
        login: bool,
        deadline: Instant,
    ) -> Result<StoredCredential, NativeCodexAuthError> {
        remaining_login_time(
            deadline,
            login_token_response_or(login, NativeCodexAuthError::Transport(None)),
        )?;
        if tokens.access_token.is_empty()
            || tokens.access_token.len() > MAX_TOKEN_BYTES
            || tokens.refresh_token.is_empty()
            || tokens.refresh_token.len() > MAX_TOKEN_BYTES
            || tokens.id_token.is_empty()
            || tokens.id_token.len() > MAX_TOKEN_BYTES
        {
            return Err(login_token_response_or(
                login,
                NativeCodexAuthError::Validation,
            ));
        }
        // RFC 6749 permits the token response to omit `scope` when it equals
        // the requested scope. The pinned route has no caller-controlled scope,
        // so retain the exact requested set instead of requiring an extension
        // field that the provider-owned Codex client does not consume.
        let scopes = validate_exact_scopes(OPENAI_CODEX_OAUTH_SCOPES)
            .map_err(|error| login_token_response_or(login, error))?;
        let mut claims = self
            .validate_id_token(&tokens.id_token, login, deadline, &mut None)
            .await?;
        remaining_login_time(
            deadline,
            login_account_verification_or(login, NativeCodexAuthError::Transport(None)),
        )?;
        if let Some((subject, account)) = expected_identity
            && (claims.sub != subject || claims.openai_auth.chatgpt_account_id != account)
        {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        self.validate_user_info(&tokens.access_token, &claims.sub, login, deadline)
            .await?;
        remaining_login_time(
            deadline,
            login_account_verification_or(login, NativeCodexAuthError::Transport(None)),
        )?;
        let account = AccountBinding::new(&claims.openai_auth.chatgpt_account_id)
            .map_err(|_| login_account_verification_or(login, NativeCodexAuthError::Validation))?;
        // Codex schedules refresh from the access token's own JWT expiry. The
        // signed ID-token expiry is a conservative fallback for an opaque
        // access token; this timing hint never supplies identity authority.
        let access_expires_at =
            scheduled_access_expiry(&tokens.access_token, claims.exp, self.clock.now().seconds())
                .map_err(|error| login_token_response_or(login, error))?;
        Ok(StoredCredential {
            access_token: std::mem::take(&mut tokens.access_token),
            refresh_token: std::mem::take(&mut tokens.refresh_token),
            id_token: std::mem::take(&mut tokens.id_token),
            subject: std::mem::take(&mut claims.sub),
            account_id: account.as_str().to_owned(),
            scopes,
            access_expires_at,
            selection_epoch: generate_selection_epoch()?,
        })
    }

    pub(super) async fn validate_refresh_response(
        &self,
        mut tokens: OAuthRefreshTokenResponse,
        mut old: StoredCredential,
        deadline: Instant,
        reason: &mut Option<CredentialRefreshFailureReason>,
    ) -> Result<StoredCredential, NativeCodexAuthError> {
        remaining_login_time(deadline, NativeCodexAuthError::Transport(None))?;
        if tokens
            .access_token
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > MAX_TOKEN_BYTES)
            || tokens
                .refresh_token
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > MAX_TOKEN_BYTES)
            || tokens
                .id_token
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > MAX_TOKEN_BYTES)
        {
            *reason = Some(CredentialRefreshFailureReason::TokenShape);
            return Err(NativeCodexAuthError::Validation);
        }
        if old.scopes != validate_exact_scopes(OPENAI_CODEX_OAUTH_SCOPES)? {
            *reason = Some(CredentialRefreshFailureReason::StoredCredential);
            return Err(NativeCodexAuthError::Validation);
        }

        let mut replacement_id_expiry = None;
        if let Some(id_token) = tokens.id_token.as_deref() {
            let claims = self
                .validate_id_token(id_token, false, deadline, reason)
                .await?;
            if claims.sub != old.subject || claims.openai_auth.chatgpt_account_id != old.account_id
            {
                *reason = Some(CredentialRefreshFailureReason::SubjectMismatch);
                return Err(NativeCodexAuthError::AccountChanged);
            }
            replacement_id_expiry = Some(claims.exp);
        }
        remaining_login_time(deadline, NativeCodexAuthError::Transport(None))?;

        let access_replaced = tokens.access_token.is_some();
        if let Some(access_token) = tokens.access_token.take() {
            old.access_token.zeroize();
            old.access_token = access_token;
        }
        if let Some(refresh_token) = tokens.refresh_token.take() {
            old.refresh_token.zeroize();
            old.refresh_token = refresh_token;
        }
        if let Some(id_token) = tokens.id_token.take() {
            old.id_token.zeroize();
            old.id_token = id_token;
        }

        if access_replaced {
            self.validate_user_info(&old.access_token, &old.subject, false, deadline)
                .await?;
            remaining_login_time(deadline, NativeCodexAuthError::Transport(None))?;
            let now = self.clock.now().seconds();
            let fallback = replacement_id_expiry.unwrap_or(
                now.checked_add(DEFAULT_ACCESS_TOKEN_LIFETIME)
                    .ok_or(NativeCodexAuthError::Validation)?,
            );
            old.access_expires_at = scheduled_access_expiry(&old.access_token, fallback, now)
                .inspect_err(|_| {
                    *reason = Some(CredentialRefreshFailureReason::AccessTokenExpired);
                })?;
        }
        Ok(old)
    }

    async fn validate_id_token(
        &self,
        id_token: &str,
        login: bool,
        deadline: Instant,
        reason: &mut Option<CredentialRefreshFailureReason>,
    ) -> Result<IdTokenClaims, NativeCodexAuthError> {
        remaining_login_time(
            deadline,
            login_account_verification_or(login, NativeCodexAuthError::Transport(None)),
        )?;
        let invalid = |reason_slot: &mut Option<CredentialRefreshFailureReason>, why| {
            *reason_slot = Some(why);
            login_account_verification_or(login, NativeCodexAuthError::Validation)
        };
        let header = decode_header(id_token)
            .map_err(|_| invalid(reason, CredentialRefreshFailureReason::IdTokenHeader))?;
        let kid = validate_jose_header(&header, login)
            .inspect_err(|_| *reason = Some(CredentialRefreshFailureReason::IdTokenHeader))?;
        let jwk = self
            .jwk_for(&kid, login, deadline)
            .await
            .inspect_err(|error| {
                if matches!(error, NativeCodexAuthError::Validation) {
                    *reason = Some(CredentialRefreshFailureReason::IdTokenUnknownKid);
                }
            })?;
        remaining_login_time(
            deadline,
            login_account_verification_or(login, NativeCodexAuthError::Transport(None)),
        )?;
        let key = DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
            .map_err(|_| invalid(reason, CredentialRefreshFailureReason::IdTokenSigningKey))?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[OPENAI_CODEX_NATIVE_CLIENT_ID]);
        validation.set_issuer(&[OPENAI_AUTH_ORIGIN.trim_end_matches('/')]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = 30;
        // The checks below enforce exp/iat/nbf against the injected clock,
        // including the stricter access-refresh lead, not jsonwebtoken's wall clock.
        validation.validate_exp = false;
        let claims = decode::<IdTokenClaims>(id_token, &key, &validation)
            .map_err(|error| invalid(reason, jwt_failure_reason(&error)))?
            .claims;
        let audience_is_exact = match &claims.aud {
            AudienceClaim::One(value) => value == OPENAI_CODEX_NATIVE_CLIENT_ID,
            AudienceClaim::Many(values) => {
                values.len() == 1 && values[0] == OPENAI_CODEX_NATIVE_CLIENT_ID
            }
        };
        let now = self.clock.now().seconds();
        let failed = if claims.iss != OPENAI_AUTH_ORIGIN.trim_end_matches('/') {
            Some(CredentialRefreshFailureReason::IdTokenIss)
        } else if !audience_is_exact {
            Some(CredentialRefreshFailureReason::IdTokenAud)
        } else if claims
            .azp
            .as_deref()
            .is_some_and(|authorized_party| authorized_party != OPENAI_CODEX_NATIVE_CLIENT_ID)
        {
            Some(CredentialRefreshFailureReason::IdTokenAzp)
        } else if claims.sub.is_empty() || claims.sub.len() > 256 {
            Some(CredentialRefreshFailureReason::IdTokenSub)
        } else if claims.iat > now.saturating_add(30) {
            Some(CredentialRefreshFailureReason::IdTokenIat)
        } else if claims
            .nbf
            .is_some_and(|not_before| not_before > now.saturating_add(30))
        {
            Some(CredentialRefreshFailureReason::IdTokenNbf)
        } else if claims.exp <= now.saturating_add(ACCESS_REFRESH_SKEW) {
            Some(CredentialRefreshFailureReason::IdTokenExp)
        } else if claims.exp <= claims.iat
            || claims.exp.saturating_sub(claims.iat) > MAX_ID_TOKEN_LIFETIME
        {
            Some(CredentialRefreshFailureReason::IdTokenClaims)
        } else {
            None
        };
        if let Some(why) = failed {
            return Err(invalid(reason, why));
        }
        remaining_login_time(
            deadline,
            login_account_verification_or(login, NativeCodexAuthError::Transport(None)),
        )?;
        AccountBinding::new(&claims.openai_auth.chatgpt_account_id)
            .map_err(|_| invalid(reason, CredentialRefreshFailureReason::IdTokenClaims))?;
        Ok(claims)
    }

    async fn validate_user_info(
        &self,
        access_token: &str,
        expected_subject: &str,
        login: bool,
        deadline: Instant,
    ) -> Result<(), NativeCodexAuthError> {
        let stage_error =
            || login_account_verification_or(login, NativeCodexAuthError::Transport(None));
        remaining_login_time(deadline, stage_error())?;
        let reply = execute_bounded(
            &self.client,
            &self.endpoints.auth_origin,
            Method::GET,
            USER_INFO_PATH,
            bearer_headers(access_token, false)
                .map_err(|error| login_account_verification_or(login, error))?,
            Vec::new(),
            capped_login_deadline(deadline, AUTH_REQUEST_CAP, stage_error())?,
        )
        .await
        .map_err(|_| login_account_verification_or(login, NativeCodexAuthError::Transport(None)))?;
        require_status(&reply, StatusCode::OK)
            .map_err(|error| login_account_verification_or(login, error))?;
        require_json_media_type(&reply)
            .map_err(|error| login_account_verification_or(login, error))?;
        let info: UserInfoResponse = parse_strict_json(&reply.body)
            .map_err(|error| login_account_verification_or(login, error))?;
        remaining_login_time(deadline, stage_error())?;
        if info.sub.is_empty()
            || info.sub.len() > 256
            || !constant_time_value_eq(expected_subject, &info.sub)
        {
            return Err(login_account_verification_or(
                login,
                NativeCodexAuthError::Validation,
            ));
        }
        Ok(())
    }

    pub(super) async fn jwk_for(
        &self,
        kid: &str,
        login: bool,
        deadline: Instant,
    ) -> Result<RsaJwk, NativeCodexAuthError> {
        self.jwks
            .resolve(
                kid,
                Some(deadline),
                || login_signing_keys_or(login, NativeCodexAuthError::Transport(None)),
                || login_signing_keys_or(login, NativeCodexAuthError::Validation),
                || self.fetch_jwks_snapshot(login, deadline),
            )
            .await
    }

    async fn fetch_jwks_snapshot(
        &self,
        login: bool,
        deadline: Instant,
    ) -> Result<FetchedJwks<RsaJwk>, NativeCodexAuthError> {
        let signing_transport =
            || login_signing_keys_or(login, NativeCodexAuthError::Transport(None));
        let signing_validation = || login_signing_keys_or(login, NativeCodexAuthError::Validation);
        remaining_login_time(deadline, signing_transport())?;
        let reply = execute_bounded(
            &self.client,
            &self.endpoints.auth_origin,
            Method::GET,
            JWKS_PATH,
            base_headers(),
            Vec::new(),
            capped_login_deadline(deadline, AUTH_REQUEST_CAP, signing_transport())?,
        )
        .await
        .map_err(|_| signing_transport())?;
        require_status(&reply, StatusCode::OK)
            .map_err(|error| login_signing_keys_or(login, error))?;
        require_json_media_type(&reply).map_err(|error| login_signing_keys_or(login, error))?;
        let document: JwksDocument =
            parse_strict_json(&reply.body).map_err(|error| login_signing_keys_or(login, error))?;
        remaining_login_time(deadline, signing_transport())?;
        if document.keys.is_empty() || document.keys.len() > MAX_JWKS_KEYS {
            return Err(signing_validation());
        }
        let mut keys = BTreeMap::new();
        for key in document.keys {
            validate_identifier(&key.kid, 128)
                .map_err(|error| login_signing_keys_or(login, error))?;
            if key.kty != "RSA"
                || key.usage != "sig"
                || key.alg != "RS256"
                || key.n.is_empty()
                || key.n.len() > 2048
                || key.e.is_empty()
                || key.e.len() > 32
                || !rsa_components_are_acceptable(&key.n, &key.e)
                || keys.insert(key.kid.clone(), key).is_some()
            {
                return Err(signing_validation());
            }
        }
        remaining_login_time(deadline, signing_transport())?;
        let maximum_age = reply
            .cache_control
            .as_deref()
            .and_then(parse_cache_control_max_age)
            .unwrap_or(Duration::from_secs(5 * 60))
            .min(JWKS_MAX_AGE);
        Ok(FetchedJwks {
            keys,
            expires_at: Instant::now() + maximum_age,
        })
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

pub(super) fn validate_jose_header(
    header: &jsonwebtoken::Header,
    login: bool,
) -> Result<String, NativeCodexAuthError> {
    if header.alg != Algorithm::RS256
        || header
            .typ
            .as_deref()
            .is_some_and(|token_type| !token_type.eq_ignore_ascii_case("JWT"))
        || header.cty.is_some()
        || header.jku.is_some()
        || header.jwk.is_some()
        || header.x5u.is_some()
        || header.x5c.is_some()
        || header.x5t.is_some()
        || header.x5t_s256.is_some()
        || header.crit.is_some()
        || header.enc.is_some()
        || header.zip.is_some()
    {
        return Err(login_account_verification_or(
            login,
            NativeCodexAuthError::Validation,
        ));
    }
    let kid = header
        .kid
        .as_deref()
        .ok_or_else(|| login_account_verification_or(login, NativeCodexAuthError::Validation))?;
    validate_identifier(kid, 128).map_err(|error| login_account_verification_or(login, error))?;
    Ok(kid.to_owned())
}

pub(super) fn validate_exact_scopes(scope: &str) -> Result<Vec<String>, NativeCodexAuthError> {
    if scope.is_empty() || scope.len() > 1024 {
        return Err(NativeCodexAuthError::Validation);
    }
    let expected: BTreeSet<&str> = OPENAI_CODEX_OAUTH_SCOPES.split_ascii_whitespace().collect();
    let mut actual = BTreeSet::new();
    for value in scope.split_ascii_whitespace() {
        validate_identifier(value, 128)?;
        if !actual.insert(value) {
            return Err(NativeCodexAuthError::Validation);
        }
    }
    if actual != expected {
        return Err(NativeCodexAuthError::Validation);
    }
    Ok(actual.into_iter().map(str::to_owned).collect())
}

pub(super) fn rsa_components_are_acceptable(modulus: &str, exponent: &str) -> bool {
    let Ok(modulus) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(modulus) else {
        return false;
    };
    let Ok(exponent) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(exponent) else {
        return false;
    };
    let Some((index, first)) = modulus
        .iter()
        .copied()
        .enumerate()
        .find(|(_, byte)| *byte != 0)
    else {
        return false;
    };
    let significant_bytes = modulus.len().saturating_sub(index);
    let modulus_bits = significant_bytes
        .saturating_sub(1)
        .saturating_mul(8)
        .saturating_add(usize::try_from(8_u32.saturating_sub(first.leading_zeros())).unwrap_or(0));
    modulus_bits >= 2048 && exponent.as_slice() == [0x01, 0x00, 0x01]
}
