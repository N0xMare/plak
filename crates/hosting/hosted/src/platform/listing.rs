//! Live API model lists with a per-key cache in the product home.
//!
//! The list is fetched with the route's own API key from its exact origin and
//! cached under `<product home>/cache/api-models/<account>-<key fingerprint>.json`
//! for six hours, like the subscription catalogs. When it cannot be fetched
//! the last cache is used, else the bundled reviewed table; the first request
//! then surfaces any provider outage. A failed fetch is remembered for five
//! minutes, so constructions offline do not each wait out the list deadline.
//! A cached list takes the same row checks as a fetched page and is read
//! without following a symlink.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::binding::PlatformProvider;

use super::PlatformCredentials;
use super::catalog::{ApiCatalogSource, ApiModelCatalog, bundled_catalog};

/// How long a fetched list is used before it is fetched again.
const API_MODEL_LIST_TTL_SECONDS: u64 = 6 * 60 * 60;
/// How long a failed fetch is remembered before the list is fetched again.
const FAILED_LIST_RETRY_SECONDS: u64 = 5 * 60;
const CACHE_VERSION: u32 = 2;

/// Whether session construction and `plak models` may fetch live lists.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApiModelLists {
    /// Cache, then the bundled table; never the network. Tests and doubles.
    #[default]
    Offline,
    /// Fetch when the cache is missing or older than the TTL.
    Live,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "provider", content = "models")]
pub(crate) enum CachedRows {
    #[serde(rename = "anthropic")]
    Anthropic(Vec<plakous::anthropic::AnthropicListedModel>),
    #[serde(rename = "openai")]
    OpenAi(Vec<plakous::openai::OpenAiListedModel>),
    #[serde(rename = "xai")]
    Xai(Vec<plakous::xai::XaiListedModel>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedList {
    version: u32,
    account: String,
    key_fingerprint: String,
    fetched_at: u64,
    rows: CachedRows,
}

pub(crate) fn cache_path(home: &Path, provider: PlatformProvider, fingerprint: &str) -> PathBuf {
    home.join("cache")
        .join("api-models")
        .join(format!("{}-{fingerprint}.json", provider.account()))
}

pub(crate) fn read_cache(
    path: &Path,
    provider: PlatformProvider,
    fingerprint: &str,
) -> Option<CachedList> {
    let bytes = read_owned_file(path)?;
    let cached: CachedList = serde_json::from_slice(&bytes).ok()?;
    (cached.version == CACHE_VERSION
        && cached.account == provider.account()
        && cached.key_fingerprint == fingerprint
        && rows_valid(provider, &cached.rows))
    .then_some(cached)
}

/// Reads a regular file without following a symlink at its final component:
/// the opened file must be the same inode `lstat` saw as a regular file.
fn read_owned_file(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    let linked = std::fs::symlink_metadata(path).ok()?;
    if !linked.file_type().is_file() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let opened = file.metadata().ok()?;
    if opened.dev() != linked.dev() || opened.ino() != linked.ino() {
        return None;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// The cache is a provider list too: its rows pass the provider's own page
/// checks (safe unique ids, bounds, well-formed fields) or it is ignored.
fn rows_valid(provider: PlatformProvider, rows: &CachedRows) -> bool {
    match (provider, rows) {
        (PlatformProvider::Anthropic, CachedRows::Anthropic(models)) => {
            plakous::anthropic::accept_anthropic_listed_models(models).is_ok()
        }
        (PlatformProvider::OpenAi, CachedRows::OpenAi(models)) => {
            plakous::openai::accept_openai_listed_models(models).is_ok()
        }
        (PlatformProvider::Xai, CachedRows::Xai(models)) => {
            plakous::xai::accept_xai_listed_models(models).is_ok()
        }
        _ => false,
    }
}

fn failure_path(path: &Path) -> PathBuf {
    path.with_extension("failed")
}

/// Whether a fetch for this cache path failed less than the retry interval ago.
fn recently_failed(path: &Path, now: u64) -> bool {
    read_owned_file(&failure_path(path))
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .is_some_and(|failed_at| now.saturating_sub(failed_at) < FAILED_LIST_RETRY_SECONDS)
}

pub(crate) fn write_cache(path: &Path, cached: &CachedList) -> std::io::Result<()> {
    write_owner_only(
        path,
        &serde_json::to_vec(cached).map_err(std::io::Error::other)?,
    )
}

/// Writes `bytes` to `path` with mode 0600 through a unique staged file and
/// an atomic rename, which replaces a symlink rather than following it.
/// Missing directories are created owner-only (0700).
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("cache path has no parent"))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let staged = unique_stage(path);
    let write_staged = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&staged, path)
    };
    if let Err(error) = write_staged() {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    Ok(())
}

fn unique_stage(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cache");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()))
}

/// What a lookup decided to use, before classification.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ListChoice<T> {
    Use(Vec<T>, ApiCatalogSource),
    Fetch,
    Bundled(String),
}

/// The cache decision: a fresh cache is used as is; a stale or missing one
/// asks for a fetch when lists are live, and otherwise a stale cache still
/// beats the bundled table.
pub(crate) fn choose<T: Clone>(
    cached: Option<&CachedList>,
    rows: impl Fn(&CachedRows) -> Option<Vec<T>>,
    lists: ApiModelLists,
    now: u64,
) -> ListChoice<T> {
    let models = cached.and_then(|cached| rows(&cached.rows));
    // A cache dated in the future has no age: it is never fresh.
    let age = cached.and_then(|cached| now.checked_sub(cached.fetched_at));
    match (models, age, lists) {
        (Some(models), Some(age), _) if age < API_MODEL_LIST_TTL_SECONDS => {
            ListChoice::Use(models, ApiCatalogSource::Cached { age_seconds: age })
        }
        (_, _, ApiModelLists::Live) => ListChoice::Fetch,
        (Some(models), Some(age), ApiModelLists::Offline) => {
            ListChoice::Use(models, ApiCatalogSource::Cached { age_seconds: age })
        }
        _ => ListChoice::Bundled("no cached model list".to_owned()),
    }
}

/// After a fetch: the fresh list, else the stale cache, else the bundled table.
pub(crate) fn after_fetch<T>(
    fetched: Result<Vec<T>, String>,
    cached: Option<Vec<T>>,
    age_seconds: u64,
) -> ListChoice<T> {
    match (fetched, cached) {
        (Ok(models), _) => ListChoice::Use(models, ApiCatalogSource::Live),
        (Err(_), Some(models)) => ListChoice::Use(models, ApiCatalogSource::Cached { age_seconds }),
        (Err(error), None) => ListChoice::Bundled(format!("model list unavailable: {error}")),
    }
}

/// The route's catalog: live list, cache or bundled table, classified.
pub async fn api_model_catalog(
    provider: PlatformProvider,
    credentials: &dyn PlatformCredentials,
    product_home: Option<&Path>,
    lists: ApiModelLists,
    now: u64,
) -> ApiModelCatalog {
    api_model_catalog_observed(provider, credentials, product_home, lists, now, None).await
}

/// [`api_model_catalog`] with the session's exchange observer, which sees the
/// listing requests when a fetch happens.
pub(crate) async fn api_model_catalog_observed(
    provider: PlatformProvider,
    credentials: &dyn PlatformCredentials,
    product_home: Option<&Path>,
    lists: ApiModelLists,
    now: u64,
    observer: Option<Arc<dyn plakous::exchange_observation::ExchangeObserver>>,
) -> ApiModelCatalog {
    match provider {
        PlatformProvider::Anthropic => {
            let source = match credentials.anthropic() {
                Ok(source) => source,
                Err(error) => return bundled_catalog(provider, error.message()),
            };
            let fingerprint = match source.current_key() {
                Ok(key) => key.fingerprint(),
                Err(error) => return bundled_catalog(provider, &error.to_string()),
            };
            let lookup = Lookup::new(provider, product_home, &fingerprint, lists, now);
            lookup
                .run(
                    async {
                        plakous::anthropic::list_anthropic_api_models(source.as_ref(), observer)
                            .await
                            .map_err(|error| error.to_string())
                    },
                    CachedRows::Anthropic,
                    |rows| match rows {
                        CachedRows::Anthropic(models) => Some(models.clone()),
                        CachedRows::OpenAi(_) | CachedRows::Xai(_) => None,
                    },
                    |models, source| super::catalog::anthropic_catalog(models, source, now),
                )
                .await
        }
        PlatformProvider::OpenAi => {
            let source = match credentials.openai() {
                Ok(source) => source,
                Err(error) => return bundled_catalog(provider, error.message()),
            };
            let reference = match super::openai_credential_reference() {
                Ok(reference) => reference,
                Err(error) => return bundled_catalog(provider, error.message()),
            };
            let fingerprint =
                match plakous::openai::openai_api_key_fingerprint(&reference, source.as_ref()) {
                    Ok(fingerprint) => fingerprint,
                    Err(error) => return bundled_catalog(provider, &error.to_string()),
                };
            let lookup = Lookup::new(provider, product_home, &fingerprint, lists, now);
            lookup
                .run(
                    async {
                        plakous::openai::list_openai_api_models(
                            &reference,
                            source.as_ref(),
                            observer,
                        )
                        .await
                        .map_err(|error| error.to_string())
                    },
                    CachedRows::OpenAi,
                    |rows| match rows {
                        CachedRows::OpenAi(models) => Some(models.clone()),
                        CachedRows::Anthropic(_) | CachedRows::Xai(_) => None,
                    },
                    super::catalog::openai_catalog,
                )
                .await
        }
        PlatformProvider::Xai => {
            let source = match credentials.xai() {
                Ok(source) => source,
                Err(error) => return bundled_catalog(provider, error.message()),
            };
            let fingerprint = match plakous::xai::xai_api_key_fingerprint(source.as_ref()) {
                Ok(fingerprint) => fingerprint,
                Err(error) => return bundled_catalog(provider, &error.to_string()),
            };
            let lookup = Lookup::new(provider, product_home, &fingerprint, lists, now);
            lookup
                .run(
                    async {
                        plakous::xai::list_xai_api_models(source.as_ref(), observer)
                            .await
                            .map_err(|error| error.to_string())
                    },
                    CachedRows::Xai,
                    |rows| match rows {
                        CachedRows::Xai(models) => Some(models.clone()),
                        CachedRows::Anthropic(_) | CachedRows::OpenAi(_) => None,
                    },
                    super::catalog::xai_catalog,
                )
                .await
        }
    }
}

/// One route's cache decision for one key.
struct Lookup {
    provider: PlatformProvider,
    fingerprint: String,
    path: Option<PathBuf>,
    cached: Option<CachedList>,
    lists: ApiModelLists,
    now: u64,
}

impl Lookup {
    fn new(
        provider: PlatformProvider,
        product_home: Option<&Path>,
        fingerprint: &str,
        lists: ApiModelLists,
        now: u64,
    ) -> Self {
        let path = product_home.map(|home| cache_path(home, provider, fingerprint));
        let cached = path
            .as_deref()
            .and_then(|path| read_cache(path, provider, fingerprint));
        Self {
            provider,
            fingerprint: fingerprint.to_owned(),
            path,
            cached,
            lists,
            now,
        }
    }

    async fn run<T: Clone>(
        self,
        fetch: impl Future<Output = Result<Vec<T>, String>>,
        wrap: fn(Vec<T>) -> CachedRows,
        rows: fn(&CachedRows) -> Option<Vec<T>>,
        classify: impl FnOnce(&[T], ApiCatalogSource) -> Result<ApiModelCatalog, String>,
    ) -> ApiModelCatalog {
        let provider = self.provider;
        let mut choice = choose(self.cached.as_ref(), rows, self.lists, self.now);
        if matches!(choice, ListChoice::Fetch) {
            let failed_recently = self
                .path
                .as_deref()
                .is_some_and(|path| recently_failed(path, self.now));
            let fetched = if failed_recently {
                Err("the last fetch failed less than five minutes ago".to_owned())
            } else {
                let fetched = fetch.await;
                self.record(&fetched, wrap);
                fetched
            };
            let stale = self.cached.as_ref().and_then(|cached| rows(&cached.rows));
            let age = self
                .cached
                .as_ref()
                .map_or(0, |cached| self.now.saturating_sub(cached.fetched_at));
            choice = after_fetch(fetched, stale, age);
        }
        match choice {
            ListChoice::Use(models, source) => {
                classify(&models, source).unwrap_or_else(|error| bundled_catalog(provider, &error))
            }
            ListChoice::Bundled(reason) => bundled_catalog(provider, &reason),
            ListChoice::Fetch => bundled_catalog(provider, "model list was not fetched"),
        }
    }

    /// Caches a fetched list, or remembers that the fetch failed.
    fn record<T: Clone>(&self, fetched: &Result<Vec<T>, String>, wrap: fn(Vec<T>) -> CachedRows) {
        let Some(path) = &self.path else {
            return;
        };
        match fetched {
            Ok(models) => {
                let _ = write_cache(
                    path,
                    &CachedList {
                        version: CACHE_VERSION,
                        account: self.provider.account().to_owned(),
                        key_fingerprint: self.fingerprint.clone(),
                        fetched_at: self.now,
                        rows: wrap(models.clone()),
                    },
                );
                let _ = std::fs::remove_file(failure_path(path));
            }
            Err(_) => {
                let _ = write_owner_only(&failure_path(path), self.now.to_string().as_bytes());
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn cached_for_test(
    provider: PlatformProvider,
    fingerprint: &str,
    fetched_at: u64,
    models: Vec<plakous::anthropic::AnthropicListedModel>,
) -> CachedList {
    CachedList {
        version: CACHE_VERSION,
        account: provider.account().to_owned(),
        key_fingerprint: fingerprint.to_owned(),
        fetched_at,
        rows: CachedRows::Anthropic(models),
    }
}
