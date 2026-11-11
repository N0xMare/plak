//! Files-first API key store (env > file > unset).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::home::paths::{PlakPaths, SecretShelf};
use plakous_hosted::account::{AccountRef, Vertical};

mod private_read;

const MAX_API_KEY_BYTES: usize = 16 * 1024;

/// API-key account ids (not OAuth subscription accounts).
#[must_use]
pub fn is_api_key_account(account: &str) -> bool {
    account
        .parse::<AccountRef>()
        .is_ok_and(|account| account.vertical().is_api())
}

#[must_use]
pub fn env_var_for_account(account: &str) -> Option<&'static str> {
    let account: AccountRef = account.parse().ok()?;
    if account.is_named() {
        return None;
    }
    match account.vertical() {
        Vertical::OpenAi => Some("OPENAI_API_KEY"),
        Vertical::XaiApi => Some("XAI_API_KEY"),
        Vertical::Anthropic => Some("ANTHROPIC_API_KEY"),
        Vertical::OpenRouter => Some("OPENROUTER_API_KEY"),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeySource {
    Env,
    File,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyStatus {
    pub account: String,
    pub source: KeySource,
    pub fingerprint: Option<String>,
    /// Set when a process environment variable is the winning source.
    pub env_var: Option<&'static str>,
    /// File-store fingerprint when an environment variable shadows it.
    pub shadowed_file_fingerprint: Option<String>,
}

/// Refuses a run whose API key is not configured, before any model is built,
/// with an authentication failure (exit 3) that names the fix.
pub fn require_api_key(paths: &PlakPaths, account: &str) -> plakous_hosted::error::CliResult<()> {
    use plakous_hosted::error::{CliFailure, CliFailureKind};
    match resolve_api_key(paths, account) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(CliFailure::classified(
            CliFailureKind::Auth,
            missing_api_key_message(account),
        )),
        Err(message) => Err(CliFailure::classified(CliFailureKind::Auth, message)),
    }
}

/// Missing-key diagnostic names the provider, `plak key set`, and its environment variable.
#[must_use]
pub fn missing_api_key_message(account: &str) -> String {
    missing_api_key_message_for(account, crate::environment::key_resolver())
}

fn missing_api_key_message_for(account: &str, resolver: crate::environment::KeyResolver) -> String {
    let (name, key_account) = match account {
        "openai" => ("OpenAI", "openai"),
        "xai-api" => ("xAI", "xai-api"),
        "anthropic" => ("Anthropic", "anthropic"),
        "openrouter" => ("OpenRouter", "openrouter"),
        other => (other, other),
    };
    let fix = match (env_var_for_account(account), resolver) {
        (Some(env), crate::environment::KeyResolver::AutoHost) => format!(
            "{name} API key is not set; run `plak key set {key_account}`; caller variable {env} is ignored"
        ),
        (Some(env), crate::environment::KeyResolver::Host) => format!(
            "{name} API key is not set; run `plak key set {key_account}` or set {env} in the host's or unit's environment"
        ),
        (Some(env), crate::environment::KeyResolver::Cli) => {
            format!("{name} API key is not set; run `plak key set {key_account}` or set {env}")
        }
        (None, _) => format!("{name} API key is not set; run `plak key set {key_account}`"),
    };
    let source = match resolver {
        crate::environment::KeyResolver::Cli => "this CLI process (environment before file store)",
        crate::environment::KeyResolver::Host => {
            "the serving host process and its own environment (including its unit environment)"
        }
        crate::environment::KeyResolver::AutoHost => {
            "the auto-started host; caller API-key variables are not inherited by its curated environment"
        }
    };
    format!("{fix}; effective key resolver: {source}")
}

/// Resolve API key: environment wins over file store.
pub fn resolve_api_key(paths: &PlakPaths, account: &str) -> Result<Option<String>, String> {
    if !is_api_key_account(account) {
        return Err(format!(
            "account `{account}` is not an API-key account; use plak login for OAuth accounts"
        ));
    }
    let selected: AccountRef = account
        .parse()
        .map_err(|error: plakous_hosted::capabilities::AccountAdmissionError| error.to_string())?;
    if let Some(name) = selected.name() {
        return Ok(
            read_shelf_secret(paths, SecretShelf::Api(selected.vertical()), name.as_str())?
                .map(|secret| secret.expose_secret().to_owned()),
        );
    }
    if let Some(name) = env_var_for_account(account)
        && std::env::var_os(name).is_some()
    {
        let value = std::env::var(name)
            .map_err(|_| format!("{name} is not valid Unicode; refusing credential fallback"))?;
        if value.is_empty() || value.len() > MAX_API_KEY_BYTES {
            return Err(format!(
                "{name} is empty or too large; refusing credential fallback"
            ));
        }
        return Ok(Some(value));
    }
    let path = paths.api_key_file(if account == "xai-api" { "xai" } else { account });
    let Some(buf) = read_api_key_file(paths, &path)? else {
        return Ok(None);
    };
    let value = buf.trim().to_owned();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(value))
}

pub fn set_api_key(
    paths: &PlakPaths,
    account: &str,
    key: zeroize::Zeroizing<String>,
) -> Result<(), String> {
    if !is_api_key_account(account) {
        return Err(format!(
            "account `{account}` is not an API-key account; use plak login for OAuth accounts"
        ));
    }
    refuse_env_override(account)?;
    if key.len() > MAX_API_KEY_BYTES {
        return Err("API key is empty or too large".into());
    }
    let key = key.trim();
    if key.is_empty() || key.len() > MAX_API_KEY_BYTES || key.chars().any(char::is_control) {
        return Err("API key is empty or too large".to_owned());
    }
    let selected: AccountRef = account
        .parse()
        .map_err(|error: plakous_hosted::capabilities::AccountAdmissionError| error.to_string())?;
    if let Some(name) = selected.name() {
        return set_shelf_secret(
            paths,
            SecretShelf::Api(selected.vertical()),
            name.as_str(),
            key,
        );
    }
    paths.ensure_layout()?;
    let path = paths.api_key_file(if account == "xai-api" { "xai" } else { account });
    write_private_file(&path, key.as_bytes())?;
    Ok(())
}

pub fn clear_api_key(paths: &PlakPaths, account: &str) -> Result<bool, String> {
    if !is_api_key_account(account) {
        return Err(format!("account `{account}` is not an API-key account"));
    }
    refuse_env_override(account)?;
    let selected: AccountRef = account
        .parse()
        .map_err(|error: plakous_hosted::capabilities::AccountAdmissionError| error.to_string())?;
    if let Some(name) = selected.name() {
        return remove_shelf_secret(paths, SecretShelf::Api(selected.vertical()), name.as_str());
    }
    let path = paths.api_key_file(if account == "xai-api" { "xai" } else { account });
    if !path.is_file() {
        return Ok(false);
    }
    fs::remove_file(&path).map_err(|_| "could not remove API key file".to_owned())?;
    Ok(true)
}

pub fn key_status(paths: &PlakPaths, account: &str) -> Result<KeyStatus, String> {
    if !is_api_key_account(account) {
        return Err(format!("account `{account}` is not an API-key account"));
    }
    if let Some(name) = env_var_for_account(account)
        && std::env::var_os(name).is_some()
    {
        let value = std::env::var(name)
            .map_err(|_| format!("{name} is not valid Unicode; refusing credential fallback"))?;
        if value.is_empty() || value.len() > MAX_API_KEY_BYTES {
            return Err(format!(
                "{name} is empty or too large; refusing credential fallback"
            ));
        }
        return Ok(KeyStatus {
            account: account.to_owned(),
            source: KeySource::Env,
            fingerprint: Some(fingerprint(&value)),
            env_var: Some(name),
            shadowed_file_fingerprint: file_fingerprint_if_present(paths, account),
        });
    }
    match resolve_api_key(paths, account)? {
        Some(value) => Ok(KeyStatus {
            account: account.to_owned(),
            source: KeySource::File,
            fingerprint: Some(fingerprint(&value)),
            env_var: None,
            shadowed_file_fingerprint: None,
        }),
        None => Ok(KeyStatus {
            account: account.to_owned(),
            source: KeySource::Missing,
            fingerprint: None,
            env_var: None,
            shadowed_file_fingerprint: None,
        }),
    }
}

#[must_use]
pub(crate) fn overriding_env_var(account: &str) -> Option<&'static str> {
    env_var_for_account(account).filter(|name| std::env::var_os(name).is_some())
}

fn refuse_env_override(account: &str) -> Result<(), String> {
    if let Some(name) = overriding_env_var(account) {
        return Err(format!(
            "{name} is set in the process environment and still wins over the file store; unset {name} (Plak never loads .env); no file change"
        ));
    }
    Ok(())
}

fn read_api_key_file(paths: &PlakPaths, path: &Path) -> Result<Option<String>, String> {
    let Some(directory) = private_read::directory(paths, &paths.secrets_dir())? else {
        return Ok(None);
    };
    let name = path.file_name().ok_or("API key file has no name")?;
    private_read::read(&directory, name)
        .map(|value| value.map(|value| value.expose_secret().to_owned()))
}

fn file_fingerprint_if_present(paths: &PlakPaths, account: &str) -> Option<String> {
    let path = paths.api_key_file(if account == "xai-api" { "xai" } else { account });
    let buf = read_api_key_file(paths, &path).ok()??;
    let value = buf.trim();
    if value.is_empty() {
        return None;
    }
    Some(fingerprint(value))
}

fn fingerprint(secret: &str) -> String {
    use std::fmt::Write as _;

    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    let hex = digest.iter().take(4).fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    });
    format!("sha256:{hex}")
}

#[cfg(feature = "web")]
pub(crate) mod observation;

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::fs::OpenOptions;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| "could not write API key file".to_owned())?;
        file.write_all(bytes)
            .map_err(|_| "could not write API key file".to_owned())?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes).map_err(|_| "could not write API key file".to_owned())?;
    }
    Ok(())
}

/// One named secret's value: zeroized on drop, never cloned or serialized,
/// and redacted in `Debug`.
pub struct NamedSecret(zeroize::Zeroizing<String>);

impl NamedSecret {
    /// Borrows the value for the shortest practical scope.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for NamedSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NamedSecret(<redacted>)")
    }
}

/// The named-secret store as hosting reads it: each MCP server spawn asks
/// for the secrets its definition names.
pub struct ProductNamedSecrets {
    paths: PlakPaths,
}

impl ProductNamedSecrets {
    #[must_use]
    pub fn shared(paths: &PlakPaths) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            paths: paths.clone(),
        })
    }
}

impl plakous_hosted::mcp_binding::NamedSecrets for ProductNamedSecrets {
    fn read(
        &self,
        name: &str,
    ) -> Result<Option<plakous_hosted::mcp_binding::NamedSecretValue>, String> {
        Ok(read_named_secret(&self.paths, name)?.map(|secret| {
            plakous_hosted::mcp_binding::NamedSecretValue::new(secret.expose_secret().to_owned())
        }))
    }

    fn read_runner(
        &self,
        name: &str,
    ) -> Result<Option<plakous_hosted::mcp_binding::NamedSecretValue>, String> {
        Ok(
            read_shelf_secret(&self.paths, SecretShelf::Runner, name)?.map(|secret| {
                plakous_hosted::mcp_binding::NamedSecretValue::new(
                    secret.expose_secret().to_owned(),
                )
            }),
        )
    }

    fn mcp_login_file(&self, server: &str) -> Option<PathBuf> {
        Some(self.paths.mcp_login_file(server))
    }
}

/// Byte bound on one named secret value.
pub const MAX_NAMED_SECRET_BYTES: usize = 16 * 1024;

/// A named secret's name: `[a-z][a-z0-9-]{0,31}`. Configuration names a
/// secret by this, never by its value.
pub fn validate_secret_name(name: &str) -> Result<(), String> {
    let valid = (1..=32).contains(&name.len())
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "secret name {name:?} must match [a-z][a-z0-9-]{{0,31}}"
        ))
    }
}

/// Stores `value` as `<home>/secrets/named/<name>.secret`, mode 0600, by an
/// exclusive temporary file renamed over any previous value.
pub fn set_named_secret(paths: &PlakPaths, name: &str, value: &str) -> Result<(), String> {
    set_shelf_secret(paths, SecretShelf::Named, name, value)
}

/// Stores `value` on `shelf`, as [`set_named_secret`] does on its own.
pub fn set_shelf_secret(
    paths: &PlakPaths,
    shelf: SecretShelf,
    name: &str,
    value: &str,
) -> Result<(), String> {
    store_shelf_secret(paths, shelf, name, value, true).map(|_| ())
}

/// Stores one secret on `shelf` only when it has none of that name: `true`
/// when this call created it, `false` when one was already there (another
/// process created it first). Never replaces a secret.
///
/// # Errors
///
/// As [`set_shelf_secret`].
pub fn create_shelf_secret(
    paths: &PlakPaths,
    shelf: SecretShelf,
    name: &str,
    value: &str,
) -> Result<bool, String> {
    store_shelf_secret(paths, shelf, name, value, false)
}

/// Writes a private staging file, then renames it over `name` (`replace`)
/// or links it in only when `name` is absent; `false` when it was present.
fn store_shelf_secret(
    paths: &PlakPaths,
    shelf: SecretShelf,
    name: &str,
    value: &str,
    replace: bool,
) -> Result<bool, String> {
    validate_secret_name(name)?;
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_NAMED_SECRET_BYTES {
        return Err(format!(
            "secret value is empty or larger than {MAX_NAMED_SECRET_BYTES} bytes"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err("secret value has a control character".to_owned());
    }
    paths.ensure_secret_shelf_dir(shelf)?;
    let path = paths.secret_shelf_file(shelf, name);
    let staging = paths
        .secret_shelf_dir(shelf)
        .join(format!(".{name}.secret.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&staging);
    let written = write_new_private_file(&staging, value.as_bytes()).and_then(|()| {
        if replace {
            return fs::rename(&staging, &path)
                .map(|()| true)
                .map_err(|_| "could not store the secret".to_owned());
        }
        // The rename fails when the name exists, so a concurrent creator's
        // secret is never replaced.
        match rustix::fs::renameat_with(
            rustix::fs::CWD,
            &staging,
            rustix::fs::CWD,
            &path,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) => Ok(true),
            Err(rustix::io::Errno::EXIST) => {
                let _ = fs::remove_file(&staging);
                Ok(false)
            }
            Err(_) => Err("could not store the secret".to_owned()),
        }
    });
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    }
    let created = written?;
    fs::File::open(paths.secret_shelf_dir(shelf))
        .and_then(|dir| dir.sync_all())
        .map_err(|_| "could not sync the secrets directory".to_owned())?;
    Ok(created)
}

/// Removes one named secret. `false` when there was none.
pub fn remove_named_secret(paths: &PlakPaths, name: &str) -> Result<bool, String> {
    remove_shelf_secret(paths, SecretShelf::Named, name)
}

/// Removes one secret from `shelf`. `false` when there was none.
pub fn remove_shelf_secret(
    paths: &PlakPaths,
    shelf: SecretShelf,
    name: &str,
) -> Result<bool, String> {
    validate_secret_name(name)?;
    if !paths.verify_secret_shelf_dir(shelf)? {
        return Ok(false);
    }
    match fs::remove_file(paths.secret_shelf_file(shelf, name)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("could not remove the secret".to_owned()),
    }
}

/// The stored secret names, sorted. Never a value.
pub fn list_named_secrets(paths: &PlakPaths) -> Result<Vec<String>, String> {
    list_shelf_secrets(paths, SecretShelf::Named)
}

/// The names stored on `shelf`, sorted. Never a value.
pub fn list_shelf_secrets(paths: &PlakPaths, shelf: SecretShelf) -> Result<Vec<String>, String> {
    list_shelf_secrets_bounded(paths, shelf, usize::MAX).map(|(names, _, _)| names)
}

/// Counts raw directory entries, including invalid names, against the bound.
pub(crate) fn list_shelf_secrets_bounded(
    paths: &PlakPaths,
    shelf: SecretShelf,
    maximum: usize,
) -> Result<(Vec<String>, usize, bool), String> {
    if !paths.verify_secret_shelf_dir(shelf)? {
        return Ok((Vec::new(), 0, false));
    }
    let entries = fs::read_dir(paths.secret_shelf_dir(shelf))
        .map_err(|_| "could not read the secrets directory".to_owned())?;
    let mut names = Vec::new();
    let mut count = 0;
    let mut truncated = false;
    for (index, entry) in entries.enumerate() {
        if index >= maximum {
            truncated = true;
            break;
        }
        let entry = entry.map_err(|_| "could not read the secrets directory".to_owned())?;
        count += 1;
        let file_name = entry.file_name();
        let Some(name) = file_name
            .to_str()
            .and_then(|file| file.strip_suffix(".secret"))
        else {
            continue;
        };
        if validate_secret_name(name).is_ok() {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok((names, count, truncated))
}

/// One named secret, or `None` when it is not stored. The directory chain and
/// the file must be private to the current user: a file other users could
/// read is refused, never used.
pub fn read_named_secret(paths: &PlakPaths, name: &str) -> Result<Option<NamedSecret>, String> {
    read_shelf_secret(paths, SecretShelf::Named, name)
}

/// One secret from `shelf`, under [`read_named_secret`]'s rules.
pub fn read_shelf_secret(
    paths: &PlakPaths,
    shelf: SecretShelf,
    name: &str,
) -> Result<Option<NamedSecret>, String> {
    validate_secret_name(name)?;
    if matches!(shelf, SecretShelf::Api(vertical) if !vertical.is_api()) {
        return Err("subscription accounts do not have API key shelves".into());
    }
    let Some(directory) = private_read::directory(paths, &paths.secret_shelf_dir(shelf))? else {
        return Ok(None);
    };
    private_read::read(&directory, std::ffi::OsStr::new(&format!("{name}.secret")))
}

/// Exclusively creates `path` 0600 and writes and syncs `bytes`.
fn write_new_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| "could not store the secret".to_owned())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "could not store the secret".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
