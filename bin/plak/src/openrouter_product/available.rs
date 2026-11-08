//! Plak's own OpenRouter credentials, and the catalog probe the CLI runs.
//!
//! The hosted binder asks an `OpenRouterCredentials` for a key source and
//! never learns where a key lives; this is the product's answer to that port
//! -- the secret store under one product home -- plus the catalog listing
//! and two-GET inspection used by `plak models` and `plak doctor`. These
//! product diagnostics are never part of a turn.

use super::OpenRouterCatalogProbe;
use std::sync::Arc;

use plakous::openrouter::{
    ApiKey, OpenRouterChatClient, OpenRouterCredentialBinding, OpenRouterCredentialRequest,
    OpenRouterKeyGeneration, OpenRouterKeySource, OpenRouterRetentionPolicy,
};

use crate::home::paths::PlakPaths;
use plakous_hosted::openrouter::OpenRouterCredentials;
use plakous_hosted::{CliFailure, CliResult};

#[derive(Clone, Default)]
pub(crate) struct UsageReceipt {
    pub(crate) dispatched: Arc<std::sync::Mutex<Option<String>>>,
}
impl UsageReceipt {
    pub(crate) fn generation(&self) -> Option<String> {
        self.dispatched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Plak's credentials: the product secret store under one product home.
pub struct ProductOpenRouterCredentials {
    paths: PlakPaths,
    usage_receipt: Option<UsageReceipt>,
}

impl ProductOpenRouterCredentials {
    /// The OpenRouter credentials of one product home.
    #[must_use]
    pub const fn new(paths: PlakPaths) -> Self {
        Self {
            paths,
            usage_receipt: None,
        }
    }

    /// A handle for a [`plakous_hosted::openrouter::ProductOpenRouterSpec`].
    #[must_use]
    pub fn shared(paths: &PlakPaths) -> Arc<dyn OpenRouterCredentials> {
        Arc::new(Self::new(paths.clone()))
    }
}

pub(crate) fn usage_credentials(
    paths: &PlakPaths,
) -> (Arc<dyn OpenRouterCredentials>, UsageReceipt) {
    let receipt = UsageReceipt::default();
    (
        Arc::new(ProductOpenRouterCredentials {
            paths: paths.clone(),
            usage_receipt: Some(receipt.clone()),
        }),
        receipt,
    )
}

pub(crate) fn key_usage_fence(
    paths: &PlakPaths,
    account: &plakous_hosted::account::AccountRef,
) -> Result<Option<String>, String> {
    if crate::home::secrets::key_status(paths, account.as_str())?.source
        == crate::home::secrets::KeySource::Missing
    {
        return Ok(None);
    }
    ProductOpenRouterApiKeySource::new(paths.clone(), account.clone())
        .map_err(|_| "key authority unavailable".to_owned())?
        .generation()
        .map(|generation| Some(generation.fingerprint().to_hex()))
        .map_err(|_| "key generation unavailable".to_owned())
}

impl OpenRouterCredentials for ProductOpenRouterCredentials {
    fn usage_read(
        &self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<plakous_hosted::openrouter::OpenRouterUsageRead> {
        let (credentials, observer) = super::usage::capture(&self.paths, account);
        Ok(plakous_hosted::openrouter::OpenRouterUsageRead {
            source: credentials.source(account)?,
            label: credentials.key_label(account)?,
            observer: Some(observer),
        })
    }
    fn key_label(&self, account: &plakous_hosted::account::AccountRef) -> CliResult<String> {
        require_openrouter(account)?;
        let status = crate::home::secrets::key_status(&self.paths, account.as_str())
            .map_err(|_| CliFailure::new("OpenRouter key store is unreadable"))?;
        key_usage_label(&status)
    }

    fn preflight(&self, account: &plakous_hosted::account::AccountRef) -> CliResult<()> {
        require_openrouter(account)?;
        crate::home::secrets::require_api_key(&self.paths, account.as_str())
    }

    fn source(
        &self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<Arc<OpenRouterKeySource>> {
        require_openrouter(account)?;
        let mut source = ProductOpenRouterApiKeySource::new(self.paths.clone(), account.clone())?;
        source.usage_receipt = self.usage_receipt.clone();
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
    usage_receipt: Option<UsageReceipt>,
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
            usage_receipt: None,
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
        if let Some(receipt) = &self.usage_receipt {
            *receipt
                .dispatched
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(expected.fingerprint().to_hex());
        }
        Ok(key)
    }
}

/// Reads model identifiers through the existing authenticated facade catalog route.
pub async fn list_models(
    paths: &PlakPaths,
    account: &plakous_hosted::account::AccountRef,
) -> CliResult<Vec<String>> {
    let credentials = ProductOpenRouterCredentials::new(paths.clone());
    credentials.preflight(account)?;
    let client = OpenRouterChatClient::connect_with_retention(
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

/// Two-GET OpenRouter catalog inspection. Never dispatches Chat/Responses/Messages.
#[must_use]
pub fn probe_openrouter_catalog_allowlist(
    paths: &PlakPaths,
    account: Option<&str>,
    model: Option<&str>,
) -> OpenRouterCatalogProbe {
    let selected = match account
        .unwrap_or("openrouter")
        .parse::<plakous_hosted::account::AccountRef>()
    {
        Ok(selected) if selected.vertical() == plakous_hosted::account::Vertical::OpenRouter => {
            selected
        }
        _ => return OpenRouterCatalogProbe::KeyPresentNoModel,
    };
    let key_present = crate::home::secrets::resolve_api_key(paths, selected.as_str())
        .ok()
        .flatten()
        .is_some();
    if !key_present {
        return OpenRouterCatalogProbe::NotConfigured;
    }
    let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) else {
        return OpenRouterCatalogProbe::KeyPresentNoModel;
    };
    if account.is_none() {
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
        let source = match ProductOpenRouterApiKeySource::new(paths, selected) {
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
    fn named_gateway_sources_bind_only_their_own_key_and_authority() {
        let directory = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(directory.path().join("home"));
        for account in ["openrouter/work", "openrouter/other"] {
            crate::home::secrets::set_api_key(&paths, account, "identical-test-key").unwrap();
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
        let credentials = ProductOpenRouterCredentials::new(paths);
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
        crate::home::secrets::set_api_key(&paths, "openrouter", "key-alpha").unwrap();
        let label = ProductOpenRouterCredentials::new(paths.clone())
            .key_label(&"openrouter".parse().unwrap())
            .unwrap();
        assert!(label.starts_with("sha256:"));
        assert_eq!(label.len(), 15);
        assert!(!label.contains("key-alpha"));
        let source =
            ProductOpenRouterApiKeySource::new(paths.clone(), "openrouter".parse().unwrap())
                .unwrap();
        let alpha = source.generation().unwrap();

        crate::home::secrets::set_api_key(&paths, "openrouter", "key-beta").unwrap();
        let beta = source.generation().unwrap();
        assert_ne!(alpha, beta);

        assert!(crate::home::secrets::clear_api_key(&paths, "openrouter").unwrap());
        assert!(source.generation().is_err());
    }
}
