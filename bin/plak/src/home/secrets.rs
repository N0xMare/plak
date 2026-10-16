//! Files-first API key store (env > file > unset).

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::Path;

use crate::home::paths::PlakPaths;

const MAX_API_KEY_BYTES: usize = 16 * 1024;

/// API-key account ids (not OAuth subscription accounts).
#[must_use]
pub fn is_api_key_account(account: &str) -> bool {
    matches!(account, "openai" | "xai-api" | "anthropic" | "openrouter")
}

#[must_use]
pub fn env_var_for_account(account: &str) -> Option<&'static str> {
    match account {
        "openai" => Some("OPENAI_API_KEY"),
        "xai-api" => Some("XAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
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
    let (name, key_account) = match account {
        "openai" => ("OpenAI", "openai"),
        "xai-api" => ("xAI", "xai-api"),
        "anthropic" => ("Anthropic", "anthropic"),
        "openrouter" => ("OpenRouter", "openrouter"),
        other => (other, other),
    };
    match env_var_for_account(account) {
        Some(env) => {
            format!("{name} API key is not set; run `plak key set {key_account}` or set {env}")
        }
        None => format!("{name} API key is not set; run `plak key set {key_account}`"),
    }
}

/// Resolve API key: environment wins over file store.
pub fn resolve_api_key(paths: &PlakPaths, account: &str) -> Result<Option<String>, String> {
    if !is_api_key_account(account) {
        return Err(format!(
            "account `{account}` is not an API-key account; use plak login for OAuth accounts"
        ));
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
    if !path.is_file() {
        return Ok(None);
    }
    let buf = read_api_key_file(&path)?;
    let value = buf.trim().to_owned();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(value))
}

pub fn set_api_key(paths: &PlakPaths, account: &str, key: &str) -> Result<(), String> {
    if !is_api_key_account(account) {
        return Err(format!(
            "account `{account}` is not an API-key account; use plak login for OAuth accounts"
        ));
    }
    refuse_env_override(account)?;
    let key = key.trim();
    if key.is_empty() || key.len() > MAX_API_KEY_BYTES {
        return Err("API key is empty or too large".to_owned());
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

fn refuse_env_override(account: &str) -> Result<(), String> {
    if let Some(name) = env_var_for_account(account)
        && std::env::var_os(name).is_some()
    {
        return Err(format!(
            "{name} is set in the process environment and still wins over the file store; unset {name} (Plak never loads .env); no file change"
        ));
    }
    Ok(())
}

fn read_api_key_file(path: &Path) -> Result<String, String> {
    let file = fs::File::open(path).map_err(|_| "could not read API key file".to_owned())?;
    let mut buf = Vec::with_capacity(MAX_API_KEY_BYTES + 1);
    file.take((MAX_API_KEY_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|_| "could not read API key file".to_owned())?;
    if buf.len() > MAX_API_KEY_BYTES {
        return Err("API key file is too large".to_owned());
    }
    String::from_utf8(buf).map_err(|_| "could not read API key file".to_owned())
}

fn file_fingerprint_if_present(paths: &PlakPaths, account: &str) -> Option<String> {
    let path = paths.api_key_file(if account == "xai-api" { "xai" } else { account });
    if !path.is_file() {
        return None;
    }
    let buf = read_api_key_file(&path).ok()?;
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
    paths.ensure_named_secrets_dir()?;
    let path = paths.named_secret_file(name);
    let staging = paths
        .named_secrets_dir()
        .join(format!(".{name}.secret.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&staging);
    let written = write_new_private_file(&staging, value.as_bytes()).and_then(|()| {
        fs::rename(&staging, &path).map_err(|_| "could not store the secret".to_owned())
    });
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    }
    written?;
    fs::File::open(paths.named_secrets_dir())
        .and_then(|dir| dir.sync_all())
        .map_err(|_| "could not sync the named secrets directory".to_owned())
}

/// Removes one named secret. `false` when there was none.
pub fn remove_named_secret(paths: &PlakPaths, name: &str) -> Result<bool, String> {
    validate_secret_name(name)?;
    if !paths.verify_named_secrets_dir()? {
        return Ok(false);
    }
    match fs::remove_file(paths.named_secret_file(name)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("could not remove the secret".to_owned()),
    }
}

/// The stored secret names, sorted. Never a value.
pub fn list_named_secrets(paths: &PlakPaths) -> Result<Vec<String>, String> {
    if !paths.verify_named_secrets_dir()? {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(paths.named_secrets_dir())
        .map_err(|_| "could not read the named secrets directory".to_owned())?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| "could not read the named secrets directory".to_owned())?;
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
    Ok(names)
}

/// One named secret, or `None` when it is not stored. The directory chain and
/// the file must be private to the current user: a file other users could
/// read is refused, never used.
pub fn read_named_secret(paths: &PlakPaths, name: &str) -> Result<Option<NamedSecret>, String> {
    validate_secret_name(name)?;
    if !paths.verify_named_secrets_dir()? {
        return Ok(None);
    }
    let path = paths.named_secret_file(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(format!("could not inspect secret {name}")),
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "secret {name} at {} is not a regular file",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let mode = metadata.permissions().mode() & 0o777;
        if metadata.uid() != rustix::process::geteuid().as_raw()
            || mode & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(format!(
                "secret {name} at {} is not a private single-link file of the current user (mode {mode:o}); run `chmod 600 {}`",
                path.display(),
                path.display()
            ));
        }
    }
    let file = fs::File::open(&path).map_err(|_| format!("could not read secret {name}"))?;
    let mut buf = zeroize::Zeroizing::new(Vec::with_capacity(MAX_NAMED_SECRET_BYTES + 1));
    file.take((MAX_NAMED_SECRET_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|_| format!("could not read secret {name}"))?;
    if buf.len() > MAX_NAMED_SECRET_BYTES {
        return Err(format!("secret {name} is too large"));
    }
    let text = std::str::from_utf8(&buf).map_err(|_| format!("secret {name} is not UTF-8"))?;
    let value = text.trim();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(NamedSecret(zeroize::Zeroizing::new(value.to_owned()))))
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
mod tests {
    use super::*;
    use crate::home::paths::PlakPaths;
    use proptest::prelude::*;
    use std::ffi::OsStr;
    use tempfile::tempdir;

    fn unshadowed_api_key_account() -> Option<&'static str> {
        ["openai", "xai-api", "anthropic", "openrouter"]
            .into_iter()
            .find(|account| {
                env_var_for_account(account).is_some_and(|name| std::env::var_os(name).is_none())
            })
    }

    #[test]
    fn file_round_trip_and_fingerprint() {
        let dir = tempdir().unwrap();
        let paths =
            PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
        paths.ensure_layout().unwrap();
        let Some(account) = unshadowed_api_key_account() else {
            let error = set_api_key(&paths, "openai", "sk-test-secret-key-9999").unwrap_err();
            assert!(error.contains("OPENAI_API_KEY"));
            assert!(error.contains("still wins over the file store"));
            return;
        };
        set_api_key(&paths, account, "sk-test-secret-key-9999").unwrap();
        let got = resolve_api_key(&paths, account).unwrap().unwrap();
        assert_eq!(got, "sk-test-secret-key-9999");
        let status = key_status(&paths, account).unwrap();
        assert_eq!(status.source, KeySource::File);
        let fingerprint = status.fingerprint.unwrap();
        assert!(fingerprint.starts_with("sha256:"));
        assert_eq!(fingerprint.len(), "sha256:".len() + 8);
        assert!(!fingerprint.contains("9999"));
        assert!(clear_api_key(&paths, account).unwrap());
    }

    #[test]
    fn xai_api_product_account_shares_only_the_xai_key_store() {
        let dir = tempdir().unwrap();
        let paths = PlakPaths::explicit(dir.path().to_owned());
        paths.ensure_layout().unwrap();
        set_api_key(&paths, "xai-api", "offline-fixture-key").unwrap();
        assert!(paths.api_key_file("xai").is_file());
        assert!(!paths.api_key_file("xai-api").exists());
        assert!(clear_api_key(&paths, "xai-api").unwrap());
        assert!(!clear_api_key(&paths, "xai-api").unwrap());
        assert_eq!(env_var_for_account("xai-api"), Some("XAI_API_KEY"));
        assert_eq!(env_var_for_account("grok"), None);
    }

    #[test]
    fn rejects_oauth_account_names() {
        let dir = tempdir().unwrap();
        let paths =
            PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
        assert!(set_api_key(&paths, "grok", "x").is_err());
        assert!(set_api_key(&paths, "codex", "x").is_err());
    }

    #[test]
    fn direct_key_file_enforces_raw_size_limit() {
        let Some(account) = unshadowed_api_key_account() else {
            return;
        };
        let dir = tempdir().unwrap();
        let paths = PlakPaths::explicit(dir.path().to_owned());
        paths.ensure_layout().unwrap();
        let path = paths.api_key_file(account);

        fs::write(&path, vec![b'x'; MAX_API_KEY_BYTES]).unwrap();
        assert_eq!(
            resolve_api_key(&paths, account).unwrap().unwrap().len(),
            MAX_API_KEY_BYTES
        );
        assert!(file_fingerprint_if_present(&paths, account).is_some());

        fs::write(&path, vec![b'x'; MAX_API_KEY_BYTES + 1]).unwrap();
        assert_eq!(
            resolve_api_key(&paths, account).unwrap_err(),
            "API key file is too large"
        );
        assert_eq!(file_fingerprint_if_present(&paths, account), None);
    }

    #[test]
    fn secret_names_follow_the_template_name_grammar() {
        for good in ["a", "github", "linear-ws", "a1-b2", &"a".repeat(32)] {
            assert_eq!(validate_secret_name(good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "1a",
            "-a",
            "A",
            "a_b",
            "a.b",
            "a/b",
            "../x",
            &"a".repeat(33),
        ] {
            assert!(validate_secret_name(bad).is_err(), "{bad}");
        }
        assert_eq!(
            validate_secret_name("Bad").unwrap_err(),
            "secret name \"Bad\" must match [a-z][a-z0-9-]{0,31}"
        );
    }

    #[test]
    fn named_secrets_are_private_files_listed_by_name_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().unwrap();
        let paths = PlakPaths::explicit(dir.path().join("home"));
        assert_eq!(list_named_secrets(&paths).unwrap(), Vec::<String>::new());
        assert!(read_named_secret(&paths, "github").unwrap().is_none());
        assert!(!remove_named_secret(&paths, "github").unwrap());

        set_named_secret(&paths, "github", "  ghp-offline-fixture\n").unwrap();
        set_named_secret(&paths, "linear", "lin-offline-fixture").unwrap();
        let file = paths.named_secret_file("github");
        assert_eq!(file, paths.secrets_dir().join("named/github.secret"));
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&paths.named_secrets_dir()), 0o700);
        assert_eq!(
            read_named_secret(&paths, "github")
                .unwrap()
                .unwrap()
                .expose_secret(),
            "ghp-offline-fixture"
        );
        let listed = list_named_secrets(&paths).unwrap();
        assert_eq!(listed, ["github", "linear"]);
        let debug = format!("{:?}", read_named_secret(&paths, "github").unwrap());
        assert!(!debug.contains("ghp-offline-fixture"), "{debug}");

        // A replacement overwrites; no staging file survives.
        set_named_secret(&paths, "github", "ghp-second").unwrap();
        assert_eq!(
            read_named_secret(&paths, "github")
                .unwrap()
                .unwrap()
                .expose_secret(),
            "ghp-second"
        );
        assert_eq!(fs::read_dir(paths.named_secrets_dir()).unwrap().count(), 2);

        // A file other users can read is refused, not used.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let refused = read_named_secret(&paths, "github").unwrap_err();
        assert!(
            refused.contains("is not a private single-link file"),
            "{refused}"
        );
        assert!(refused.contains("chmod 600"), "{refused}");
        // So is a permissive named directory.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(paths.named_secrets_dir(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            read_named_secret(&paths, "github")
                .unwrap_err()
                .contains("chmod 700")
        );
        assert!(list_named_secrets(&paths).is_err());
        assert!(set_named_secret(&paths, "github", "x").is_err());
        fs::set_permissions(paths.named_secrets_dir(), fs::Permissions::from_mode(0o700)).unwrap();

        // A hard link could alias the secret elsewhere: refused too.
        let alias = dir.path().join("alias");
        fs::hard_link(&file, &alias).unwrap();
        assert!(
            read_named_secret(&paths, "github")
                .unwrap_err()
                .contains("single-link")
        );
        fs::remove_file(&alias).unwrap();

        assert!(remove_named_secret(&paths, "github").unwrap());
        assert!(!remove_named_secret(&paths, "github").unwrap());
        assert_eq!(list_named_secrets(&paths).unwrap(), ["linear"]);

        for (name, value) in [("Bad", "v"), ("ok", ""), ("ok", "a\u{7}b")] {
            assert!(set_named_secret(&paths, name, value).is_err(), "{name}");
        }
        assert!(set_named_secret(&paths, "big", &"x".repeat(MAX_NAMED_SECRET_BYTES + 1)).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn api_key_file_round_trip_prop(suffix in "[a-zA-Z0-9]{8,32}") {
            let dir = tempdir().unwrap();
            let paths = PlakPaths::resolve(
                Some(OsStr::new(dir.path())),
                Some(OsStr::new("/home/u")),
            )
            .unwrap();
            paths.ensure_layout().unwrap();
            let key = format!("sk-test-{suffix}");
            if let Some(account) = unshadowed_api_key_account() {
                set_api_key(&paths, account, &key).unwrap();
                let got = resolve_api_key(&paths, account).unwrap().unwrap();
                prop_assert_eq!(got, key.clone());
                let status = key_status(&paths, account).unwrap();
                prop_assert_eq!(status.source, KeySource::File);
                let fp = status.fingerprint.unwrap();
                prop_assert!(!fp.contains(&key));
                prop_assert!(clear_api_key(&paths, account).unwrap());
                prop_assert!(resolve_api_key(&paths, account).unwrap().is_none());
            } else {
                let error = set_api_key(&paths, "openai", &key).unwrap_err();
                prop_assert!(error.contains("OPENAI_API_KEY"));
            }
        }
    }
}
