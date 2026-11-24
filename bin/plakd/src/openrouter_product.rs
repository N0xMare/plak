//! Plak's own OpenRouter credentials, and the catalog probe the CLI runs.
//!
//! The hosted binder asks an `OpenRouterCredentials` for a key source and
//! never learns where a key lives; this is the product's answer to that port
//! -- the secret store under one product home -- plus the catalog listing
//! and two-GET inspection used by `plakd models` and `plakd doctor`. These
//! product diagnostics are never part of a turn.

use std::sync::Arc;

use plakous::openrouter::{
    ApiKey, OpenRouterChatClient, OpenRouterCredentialBinding, OpenRouterCredentialRequest,
    OpenRouterKeyGeneration, OpenRouterKeySource, OpenRouterRetentionPolicy,
};

use crate::home::paths::PlakPaths;
use plakous_hosted::openrouter::OpenRouterCredentials;
use plakous_hosted::{CliFailure, CliResult};

pub(crate) mod usage;
pub(crate) use usage::legacy as legacy_key_usage;
pub(crate) use usage::read_only as read_key_usage;

/// Plak's credentials: the product secret store under one product home.
pub struct ProductOpenRouterCredentials {
    paths: PlakPaths,
    custody: crate::gateway::HostCustody,
}

impl ProductOpenRouterCredentials {
    /// The OpenRouter credentials of one product home under `custody`.
    #[must_use]
    pub(crate) const fn new(paths: PlakPaths, custody: crate::gateway::HostCustody) -> Self {
        Self { paths, custody }
    }

    /// A handle for a [`plakous_hosted::openrouter::ProductOpenRouterSpec`].
    #[must_use]
    pub(crate) fn shared(
        paths: &PlakPaths,
        custody: crate::gateway::HostCustody,
    ) -> Arc<dyn OpenRouterCredentials> {
        Arc::new(Self::new(paths.clone(), custody))
    }
}

impl OpenRouterCredentials for ProductOpenRouterCredentials {
    fn custody(&self) -> plakous::custody::Custody {
        self.custody.route_custody()
    }

    fn key_label(&self, account: &plakous_hosted::account::AccountRef) -> CliResult<String> {
        require_openrouter(account)?;
        if self.custody.gateway().is_some() {
            return Ok(account.as_str().to_owned());
        }
        let status = crate::home::secrets::key_status(&self.paths, account.as_str())
            .map_err(|_| CliFailure::new("OpenRouter key store is unreadable"))?;
        key_usage_label(&status)
    }

    fn preflight(&self, account: &plakous_hosted::account::AccountRef) -> CliResult<()> {
        require_openrouter(account)?;
        if self.custody.gateway().is_some() {
            return Ok(());
        }
        crate::home::secrets::require_api_key(&self.paths, account.as_str())
    }

    fn source(
        &self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<Arc<OpenRouterKeySource>> {
        require_openrouter(account)?;
        if let Some(gateway) = self.custody.gateway() {
            let authority = OpenRouterCredentialBinding::platform(
                "plak.openrouter",
                account.source_authority("plak.product-secrets.openrouter.v1"),
            )
            .map_err(|_| CliFailure::new("OpenRouter credential binding is invalid"))?;
            return Ok(Arc::new(
                gateway
                    .key_source(
                        account.as_str(),
                        plakous::api_key::CredentialProviderTag::OpenRouterPlatform,
                        "https://openrouter.ai",
                        None,
                        authority,
                    )
                    .map_err(|error| CliFailure::new(error.to_string()))?,
            ));
        }
        let source = ProductOpenRouterApiKeySource::new(self.paths.clone(), account.clone())?;
        Ok(Arc::new(source))
    }
}

fn require_openrouter(account: &plakous_hosted::account::AccountRef) -> CliResult<()> {
    if account.vertical() != plakous_hosted::account::Vertical::OpenRouter {
        return Err(
            plakous_hosted::capabilities::AccountAdmissionError::AccountTargetMismatch {
                account: account.to_string(),
            }
            .into(),
        );
    }
    Ok(())
}

pub(crate) fn gateway_usage(
    view: serde_json::Value,
    observed_ms: u64,
    account: &str,
) -> CliResult<plakous_hosted::subscription_usage::VerticalObservation> {
    let nanos = u32::try_from(observed_ms % 1000).unwrap_or(0) * 1_000_000;
    let observed = plakous::model::Timestamp::new(observed_ms / 1000, nanos)
        .map_err(|_| CliFailure::new("invalid gateway observation time"))?;
    let body = serde_json::to_vec(&std::collections::BTreeMap::from([("data", view)]))
        .map_err(|_| CliFailure::new("invalid gateway allowance view"))?;
    let usage = plakous::openrouter::decode_openrouter_key_usage(&body, observed)
        .map_err(|_| CliFailure::new("invalid gateway OpenRouter allowance"))?;
    Ok(
        plakous_hosted::subscription_usage::VerticalObservation::OpenRouterKey(
            Box::new(usage),
            account.to_owned(),
        ),
    )
}

fn key_usage_label(status: &crate::home::secrets::KeyStatus) -> CliResult<String> {
    use crate::home::secrets::KeySource;
    match status.source {
        KeySource::Env => status.env_var.map(str::to_owned),
        KeySource::File => status.fingerprint.clone(),
        KeySource::Missing => None,
    }
    .ok_or_else(|| CliFailure::new("OpenRouter API key is unavailable"))
}

/// Product-owned credential authority. It retains only the stable account binding;
/// every authority check and physical dispatch re-resolves the current key.
struct ProductOpenRouterApiKeySource {
    paths: PlakPaths,
    account: plakous_hosted::account::AccountRef,
    binding: OpenRouterCredentialBinding,
}

impl ProductOpenRouterApiKeySource {
    fn new(paths: PlakPaths, account: plakous_hosted::account::AccountRef) -> CliResult<Self> {
        let binding = OpenRouterCredentialBinding::platform(
            "plak.openrouter",
            account.source_authority("plak.product-secrets.openrouter.v1"),
        )
        .map_err(|_| CliFailure::new("OpenRouter credential binding is invalid"))?;
        Ok(Self {
            paths,
            account,
            binding,
        })
    }

    fn current(&self) -> Result<ApiKey, plakous::api_key::ApiKeyError> {
        let value = crate::home::secrets::resolve_api_key(&self.paths, self.account.as_str())
            .map_err(|_| plakous::api_key::ApiKeyError::Unavailable)?
            .ok_or(plakous::api_key::ApiKeyError::Unavailable)?;
        ApiKey::new(value).map_err(|_| plakous::api_key::ApiKeyError::Unavailable)
    }

    fn generation(&self) -> Result<OpenRouterKeyGeneration, plakous::api_key::ApiKeyError> {
        let value = crate::home::secrets::resolve_api_key(&self.paths, self.account.as_str())
            .map_err(|_| plakous::api_key::ApiKeyError::Unavailable)?
            .ok_or(plakous::api_key::ApiKeyError::Unavailable)?;
        OpenRouterKeyGeneration::from_generation_binding(
            plakous::api_key::CredentialProviderTag::OpenRouterPlatform,
            self.binding.clone(),
            value.as_bytes(),
        )
    }
}

impl plakous::api_key::ApiKeySource<OpenRouterCredentialRequest<'_>, OpenRouterCredentialBinding>
    for ProductOpenRouterApiKeySource
{
    fn authority(&self) -> &OpenRouterCredentialBinding {
        &self.binding
    }
    fn generation_grant(
        &self,
        selection: &dyn plakous::api_key::ApiKeyRequest,
    ) -> Result<OpenRouterKeyGeneration, plakous::api_key::ApiKeyError> {
        if selection.provider() != plakous::api_key::CredentialProviderTag::OpenRouterPlatform
            || selection.origin() != "https://openrouter.ai"
            || selection.credential_reference().is_some()
        {
            return Err(plakous::api_key::ApiKeyError::BindingMismatch);
        }
        self.generation()
    }
    fn api_key(
        &self,
        _: &OpenRouterCredentialRequest<'_>,
        expected: &OpenRouterKeyGeneration,
    ) -> Result<ApiKey, plakous::api_key::ApiKeyError> {
        if expected.provider() != plakous::api_key::CredentialProviderTag::OpenRouterPlatform
            || expected.authority() != &self.binding
        {
            return Err(plakous::api_key::ApiKeyError::BindingMismatch);
        }
        let key = self.current()?;
        if key.generation_grant(
            plakous::api_key::CredentialProviderTag::OpenRouterPlatform,
            self.binding.clone(),
        ) != *expected
        {
            return Err(plakous::api_key::ApiKeyError::BindingMismatch);
        }
        Ok(key)
    }
}

/// Reads model identifiers through the existing authenticated facade catalog route.
pub(crate) async fn list_models(
    paths: &PlakPaths,
    custody: crate::gateway::HostCustody,
    account: &plakous_hosted::account::AccountRef,
) -> CliResult<Vec<String>> {
    let credentials = ProductOpenRouterCredentials::new(paths.clone(), custody);
    credentials.preflight(account)?;
    let client = OpenRouterChatClient::connect_with_retention_in(
        &credentials.custody(),
        credentials.source(account)?,
        OpenRouterRetentionPolicy::gateway_default(),
    )
    .and_then(OpenRouterChatClient::with_plak_attribution)
    .map_err(|error| CliFailure::new(format!("OpenRouter connection is invalid: {error}")))?;
    client
        .list_models()
        .await
        .map_err(|error| CliFailure::new(error.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn usage_key_labels_are_variable_names_or_file_fingerprints() {
        use crate::home::secrets::{KeySource, KeyStatus};
        let mut status = KeyStatus {
            account: "openrouter".to_owned(),
            source: KeySource::Env,
            fingerprint: Some("sha256:1234abcd".to_owned()),
            env_var: Some("OPENROUTER_API_KEY"),
            shadowed_file_fingerprint: Some("sha256:5678abcd".to_owned()),
        };
        assert_eq!(key_usage_label(&status).unwrap(), "OPENROUTER_API_KEY");
        status.source = KeySource::File;
        status.env_var = None;
        assert_eq!(key_usage_label(&status).unwrap(), "sha256:1234abcd");
        status.source = KeySource::Missing;
        assert!(key_usage_label(&status).is_err());
    }

    #[test]
    fn named_gateway_sources_bind_only_their_own_key_and_authority() {
        let directory = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(directory.path().join("home"));
        for account in ["openrouter/work", "openrouter/other"] {
            crate::home::secrets::set_api_key(
                &paths,
                account,
                zeroize::Zeroizing::new(("identical-test-key").to_string()),
            )
            .unwrap();
        }
        let work =
            ProductOpenRouterApiKeySource::new(paths.clone(), "openrouter/work".parse().unwrap())
                .unwrap();
        let other =
            ProductOpenRouterApiKeySource::new(paths.clone(), "openrouter/other".parse().unwrap())
                .unwrap();
        assert_ne!(work.binding, other.binding);
        assert_ne!(work.generation().unwrap(), other.generation().unwrap());
        crate::home::secrets::clear_api_key(&paths, "openrouter/work").unwrap();
        assert!(work.generation().is_err());
        assert!(other.generation().is_ok());
        let credentials =
            ProductOpenRouterCredentials::new(paths, crate::gateway::HostCustody::Local);
        assert!(credentials.source(&"openai/work".parse().unwrap()).is_err());
    }

    #[test]
    fn product_key_source_observes_rotation_and_revocation() {
        if std::env::var_os("OPENROUTER_API_KEY").is_some() {
            // Env wins over the file store; file rotation is unobservable here.
            return;
        }
        let dir = PlakPaths::isolated_tempdir();
        let paths = PlakPaths::resolve(
            Some(OsStr::new(dir.path())),
            Some(OsStr::new("/home/tester")),
        )
        .unwrap();
        crate::home::secrets::set_api_key(
            &paths,
            "openrouter",
            zeroize::Zeroizing::new(("key-alpha").to_string()),
        )
        .unwrap();
        let label =
            ProductOpenRouterCredentials::new(paths.clone(), crate::gateway::HostCustody::Local)
                .key_label(&"openrouter".parse().unwrap())
                .unwrap();
        assert!(label.starts_with("sha256:"));
        assert_eq!(label.len(), 15);
        assert!(!label.contains("key-alpha"));
        let source =
            ProductOpenRouterApiKeySource::new(paths.clone(), "openrouter".parse().unwrap())
                .unwrap();
        let alpha = source.generation().unwrap();

        crate::home::secrets::set_api_key(
            &paths,
            "openrouter",
            zeroize::Zeroizing::new(("key-beta").to_string()),
        )
        .unwrap();
        let beta = source.generation().unwrap();
        assert_ne!(alpha, beta);

        assert!(crate::home::secrets::clear_api_key(&paths, "openrouter").unwrap());
        assert!(source.generation().is_err());
    }
}
