//! Plak's own OpenRouter credentials, and the catalog probe the CLI runs.
//!
//! The hosted binder asks an `OpenRouterCredentials` for a key source and
//! never learns where a key lives; this is the product's answer to that port
//! -- the secret store under one product home -- plus the catalog listing
//! and two-GET inspection used by `plak models` and `plak doctor`. These
//! product diagnostics are never part of a turn.

use std::sync::Arc;

use plakous::openrouter::{
    OpenRouterApiKey, OpenRouterApiKeyGenerationGrant, OpenRouterApiKeySource, OpenRouterAuthError,
    OpenRouterCatalogCredentialRequest, OpenRouterChatClient, OpenRouterChatCredentialRequest,
    OpenRouterCredentialBinding, OpenRouterCredentialRequest, OpenRouterMessagesCredentialRequest,
    OpenRouterRetentionPolicy,
};

use crate::home::paths::PlakPaths;
use plakous_hosted::openrouter::OpenRouterCredentials;
use plakous_hosted::{CliFailure, CliResult};

/// Plak's credentials: the product secret store under one product home.
pub struct ProductOpenRouterCredentials {
    paths: PlakPaths,
}

impl ProductOpenRouterCredentials {
    /// The OpenRouter credentials of one product home.
    #[must_use]
    pub const fn new(paths: PlakPaths) -> Self {
        Self { paths }
    }

    /// A handle for a [`plakous_hosted::openrouter::ProductOpenRouterSpec`].
    #[must_use]
    pub fn shared(paths: &PlakPaths) -> Arc<dyn OpenRouterCredentials> {
        Arc::new(Self::new(paths.clone()))
    }
}

impl OpenRouterCredentials for ProductOpenRouterCredentials {
    fn key_label(&self) -> CliResult<String> {
        let status = crate::home::secrets::key_status(&self.paths, "openrouter")
            .map_err(|_| CliFailure::new("OpenRouter key store is unreadable"))?;
        key_usage_label(&status)
    }

    fn preflight(&self) -> CliResult<()> {
        crate::home::secrets::require_api_key(&self.paths, "openrouter")
    }

    fn source(&self) -> CliResult<Arc<dyn OpenRouterApiKeySource>> {
        Ok(Arc::new(ProductOpenRouterApiKeySource::new(
            self.paths.clone(),
        )?))
    }
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
    binding: OpenRouterCredentialBinding,
}

impl ProductOpenRouterApiKeySource {
    fn new(paths: PlakPaths) -> CliResult<Self> {
        let binding = OpenRouterCredentialBinding::platform(
            "plak.openrouter",
            "plak.product-secrets.openrouter.v1",
        )
        .map_err(|_| CliFailure::new("OpenRouter credential binding is invalid"))?;
        Ok(Self { paths, binding })
    }

    fn current(&self) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        let value = crate::home::secrets::resolve_api_key(&self.paths, "openrouter")
            .map_err(|_| OpenRouterAuthError::CredentialSource)?
            .ok_or(OpenRouterAuthError::CredentialSource)?;
        OpenRouterApiKey::new(value).map_err(|_| OpenRouterAuthError::CredentialSource)
    }

    fn generation(&self) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        let value = crate::home::secrets::resolve_api_key(&self.paths, "openrouter")
            .map_err(|_| OpenRouterAuthError::CredentialSource)?
            .ok_or(OpenRouterAuthError::CredentialSource)?;
        OpenRouterApiKeyGenerationGrant::from_generation_binding(value.as_bytes())
    }
}

impl OpenRouterApiKeySource for ProductOpenRouterApiKeySource {
    fn credential_binding(&self) -> Option<&OpenRouterCredentialBinding> {
        Some(&self.binding)
    }

    fn resolve(
        &self,
        _: &OpenRouterCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        self.current()
    }

    fn resolve_chat(
        &self,
        _: &OpenRouterChatCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        self.current()
    }

    fn resolve_messages(
        &self,
        _: &OpenRouterMessagesCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        self.current()
    }

    fn resolve_catalog(
        &self,
        _: &OpenRouterCatalogCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        self.current()
    }

    fn current_generation_grant(
        &self,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        self.generation()
    }

    fn authority_id(&self) -> &'static str {
        "plak.product-secrets.openrouter.v1"
    }
}

/// Reads model identifiers through the existing authenticated facade catalog route.
pub async fn list_models(paths: &PlakPaths) -> CliResult<Vec<String>> {
    let credentials = ProductOpenRouterCredentials::new(paths.clone());
    credentials.preflight()?;
    let client = OpenRouterChatClient::connect_with_retention(
        credentials.source()?,
        OpenRouterRetentionPolicy::gateway_default(),
    )
    .and_then(OpenRouterChatClient::with_plak_attribution)
    .map_err(|error| CliFailure::new(format!("OpenRouter connection is invalid: {error}")))?;
    client
        .list_models()
        .await
        .map_err(|error| CliFailure::new(error.to_string()))
}

/// Discovery-only catalog allowlist probe. No inference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenRouterCatalogProbe {
    /// No OpenRouter key in the product secret store.
    NotConfigured,
    /// Key is present but doctor has no OpenRouter default model to discover.
    KeyPresentNoModel,
    /// Authenticated models+endpoints documents validated.
    Ok,
    /// Named catalog/schema failure or transport/auth error (no secret material).
    Failed(String),
}

/// Two-GET OpenRouter catalog inspection. Never dispatches Chat/Responses/Messages.
#[must_use]
pub fn probe_openrouter_catalog_allowlist(
    paths: &PlakPaths,
    account: Option<&str>,
    model: Option<&str>,
) -> OpenRouterCatalogProbe {
    let key_present = crate::home::secrets::resolve_api_key(paths, "openrouter")
        .ok()
        .flatten()
        .is_some();
    if !key_present {
        return OpenRouterCatalogProbe::NotConfigured;
    }
    let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) else {
        return OpenRouterCatalogProbe::KeyPresentNoModel;
    };
    if account != Some("openrouter") {
        return OpenRouterCatalogProbe::KeyPresentNoModel;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return OpenRouterCatalogProbe::Failed(error.to_string()),
    };
    let paths = paths.clone();
    let model = model.to_owned();
    runtime.block_on(async move {
        let source = match ProductOpenRouterApiKeySource::new(paths) {
            Ok(source) => source,
            Err(error) => return OpenRouterCatalogProbe::Failed(error.message().to_owned()),
        };
        let client = match OpenRouterChatClient::connect_with_retention(
            Arc::new(source),
            OpenRouterRetentionPolicy::gateway_default(),
        )
        .and_then(OpenRouterChatClient::with_plak_attribution)
        {
            Ok(client) => client,
            Err(error) => {
                return OpenRouterCatalogProbe::Failed(format!(
                    "OpenRouter connection is invalid: {error}"
                ));
            }
        };
        match client.discover_exact(&model).await {
            Ok(_) => OpenRouterCatalogProbe::Ok,
            Err(error) => OpenRouterCatalogProbe::Failed(error.to_string()),
        }
    })
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
        crate::home::secrets::set_api_key(&paths, "openrouter", "key-alpha").unwrap();
        let label = ProductOpenRouterCredentials::new(paths.clone())
            .key_label()
            .unwrap();
        assert!(label.starts_with("sha256:"));
        assert_eq!(label.len(), 15);
        assert!(!label.contains("key-alpha"));
        let source = ProductOpenRouterApiKeySource::new(paths.clone()).unwrap();
        let alpha = source.current_generation_grant().unwrap();

        crate::home::secrets::set_api_key(&paths, "openrouter", "key-beta").unwrap();
        let beta = source.current_generation_grant().unwrap();
        assert_ne!(alpha, beta);

        assert!(crate::home::secrets::clear_api_key(&paths, "openrouter").unwrap());
        assert!(source.current_generation_grant().is_err());
    }
}
