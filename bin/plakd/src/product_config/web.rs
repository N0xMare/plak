//! `[web]`: the tailnet logins the maintainer web endpoint admits and the
//! exact origin it is served at. Home-scoped, so a project file can never
//! grant access, and forward-only: written only when set.

use super::keys::{ConfigKey, FieldValue};

/// The sentence for an `allow` that is not an array of strings.
const ALLOW_MUST_BE_AN_ARRAY: &str = "[web] allow must be an array like [\"you@example.com\"]";
/// The longest login the endpoint compares.
const MAX_LOGIN_BYTES: usize = 256;

pub(super) fn validate_switches(config: &super::ProductConfig) -> Result<(), String> {
    if config.web_credentials_write && !config.web_accounts_read {
        return Err("web.credentials_write requires web.accounts_read".into());
    }
    if config.web_config_write && !config.web_config_read {
        return Err("web.config_write requires web.config_read".into());
    }
    Ok(())
}

/// `[web] allow`: each login non-empty, at most 256 bytes, printable with
/// no spaces, and named once. An empty list admits no one.
pub(super) fn assigned_logins(value: FieldValue<'_>) -> Result<Vec<String>, String> {
    let logins = value.strings(ALLOW_MUST_BE_AN_ARRAY)?;
    for (index, login) in logins.iter().enumerate() {
        if login.is_empty()
            || login.len() > MAX_LOGIN_BYTES
            || !login.chars().all(|character| character.is_ascii_graphic())
        {
            return Err(format!(
                "[web] allow: {login:?} is not a tailnet login (printable ASCII, no spaces, at most {MAX_LOGIN_BYTES} bytes)"
            ));
        }
        if logins[..index].contains(login) {
            return Err(format!("[web] allow names {login} twice"));
        }
    }
    Ok(logins)
}

/// The value `[web] allow` renders, or `None` while it names no one.
pub(super) fn allow_value(logins: &[String]) -> Option<toml_edit::Value> {
    (!logins.is_empty()).then(|| logins.iter().map(String::as_str).collect())
}

/// `[web] origin`; an empty command-line value clears it.
pub(super) fn assigned_origin(
    key: &ConfigKey,
    value: FieldValue<'_>,
) -> Result<Option<String>, String> {
    let text = value.string(key)?;
    if matches!(value, FieldValue::Cli(_)) && text.is_empty() {
        return Ok(None);
    }
    origin_authority(text).map(|_| Some(text.to_owned()))
}

/// The `Host` authority of `value` when it is an exact origin: a scheme, a
/// host and an optional port, nothing more; `https`, or `http` for a
/// loopback host only.
///
/// # Errors
///
/// Anything but the origin's own serialization (a path, query, fragment,
/// user info, trailing slash, default port or upper case), or another
/// scheme.
pub(crate) fn origin_authority(value: &str) -> Result<String, String> {
    let refused = || {
        format!("[web] origin {value} is not an origin: https://<host>[:port], nothing after it")
    };
    let url = url::Url::parse(value).map_err(|_| refused())?;
    let host = url.host_str().ok_or_else(refused)?;
    let loopback = matches!(host, "127.0.0.1" | "localhost");
    let scheme = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    if !scheme
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.origin().ascii_serialization() != value
    {
        return Err(refused());
    }
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
