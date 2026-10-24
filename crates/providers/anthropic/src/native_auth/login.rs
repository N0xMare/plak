//! The paste-complete Claude OAuth login.

use super::{
    Arc, CLAUDE_OAUTH_REDIRECT_URI, ClaudeNativeAuthError, ClaudeSubscriptionStatus,
    CredentialFence, Instant, ManagerCore, PkceS256, claude_oauth_scope_admits_inference,
    claude_token_code_body, fmt, parse_claude_oauth_authorization_input,
    parse_claude_oauth_token_grant, until_flow_deadline,
};

/// Non-cloneable paste-complete Claude OAuth login.
pub struct ClaudeOAuthLogin {
    pub(super) core: Arc<ManagerCore>,
    pub(super) baseline: Option<CredentialFence>,
    pub(super) pkce: PkceS256,
    pub(super) authorization_url: String,
    pub(super) deadline: Instant,
}

impl ClaudeOAuthLogin {
    /// Returns the exact authorization URL the app should open.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// Validates pasted authorization input and exchanges the code for tokens.
    ///
    /// # Errors
    ///
    /// Fails closed for callback, deadline, token, scope, or storage errors.
    pub async fn complete(
        self,
        authorization_input: &str,
    ) -> Result<ClaudeSubscriptionStatus, ClaudeNativeAuthError> {
        self.core.ensure_flow_active(self.deadline)?;
        let verifier = self.pkce.verifier().expose_secret();
        let code = parse_claude_oauth_authorization_input(authorization_input, verifier)
            .ok_or(ClaudeNativeAuthError::InvalidCallback)?;
        let body = claude_token_code_body(&code, CLAUDE_OAUTH_REDIRECT_URI, verifier, verifier);
        let response = until_flow_deadline(self.deadline, self.core.post_json(body)).await?;
        let grant = parse_claude_oauth_token_grant(&response)
            .map_err(|_| ClaudeNativeAuthError::LoginTokenExchange)?;
        if grant
            .scope()
            .is_none_or(|scope| !claude_oauth_scope_admits_inference(scope))
        {
            return Err(ClaudeNativeAuthError::LoginAccountVerification);
        }
        if grant.refresh_token().is_none() {
            return Err(ClaudeNativeAuthError::LoginTokenExchange);
        }
        self.core.ensure_flow_active(self.deadline)?;
        self.core.persist_grant(self.baseline.as_ref(), &grant)
    }
}

impl fmt::Debug for ClaudeOAuthLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaudeOAuthLogin(<redacted non-cloneable login>)")
    }
}
