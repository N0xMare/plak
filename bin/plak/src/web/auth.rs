//! The gate every `plak web` request passes, and the router behind it.
//!
//! Identity is the tailnet's: `tailscale serve` stamps `Tailscale-User-Login`
//! on what it proxies, and the header is trusted only on the private unix
//! listener Serve proxies to, against `[web] allow`. On a loopback listener
//! any local user could forge it, so loopback is dev mode: one configured
//! login, and a warning at start. Every request must name the exact `Host`
//! of the served origin. `/wire` also needs the exact `Origin` and the
//! session cookie this gate issued to that login. Whatever fails gets the
//! same `403`; the reason goes only to the log, which never holds a cookie,
//! a key or a frame.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use plakous_hosted::error::{CliFailure, CliResult};
use ring::hmac;
use ring::rand::{SecureRandom as _, SystemRandom};
use zeroize::Zeroizing;

use super::Served;
use crate::home::paths::{PlakPaths, SecretShelf};
use crate::product_config::web::origin_authority;

/// The session cookie's name: `__Host-` binds it to this exact origin.
const COOKIE: &str = "__Host-plak";
/// How long one session cookie lasts.
const COOKIE_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
/// The cookie key's name on the web secrets shelf.
const COOKIE_KEY_NAME: &str = "cookie-key";
/// The tailnet identity header `tailscale serve` sets.
const IDENTITY: &str = "tailscale-user-login";
/// Connections one login may hold at once.
const MAX_CONNECTIONS_PER_LOGIN: usize = 4;
/// Upgrades one login may make per minute.
const MAX_UPGRADES_PER_MINUTE: usize = 30;
/// Refusals logged per minute; past it, refusals are counted, not logged.
const MAX_LOGGED_REFUSALS_PER_MINUTE: usize = 60;
const MINUTE: Duration = Duration::from_secs(60);

/// Who may use the endpoint, and the origin it is served at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WebAccess {
    /// The tailnet deployment: logins from `[web] allow`, stamped by Serve.
    Tailnet {
        allow: Vec<String>,
        origin: WebOrigin,
    },
    /// Dev mode on loopback: one configured login, no header trusted.
    Dev { login: String, origin: WebOrigin },
}

impl WebAccess {
    fn origin(&self) -> &WebOrigin {
        match self {
            Self::Tailnet { origin, .. } | Self::Dev { origin, .. } => origin,
        }
    }
}

/// The exact origin the endpoint is served at, and the `Host` it implies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WebOrigin {
    text: String,
    authority: String,
}

impl WebOrigin {
    /// Parses an exact origin (chapter 07 `[web] origin`).
    ///
    /// # Errors
    ///
    /// A value that is not exactly an origin.
    pub(crate) fn parse(value: &str) -> CliResult<Self> {
        Ok(Self {
            authority: origin_authority(value).map_err(CliFailure::new)?,
            text: value.to_owned(),
        })
    }

    fn as_str(&self) -> &str {
        &self.text
    }

    fn authority(&self) -> &str {
        &self.authority
    }
}

/// The key session cookies are signed with: 32 random bytes, hex, on the
/// web secrets shelf (`secrets/web/cookie-key.secret`, 0600, verified on
/// read). Never logged or exported; replacing it logs every client out.
pub(crate) struct CookieKey(hmac::Key);

impl CookieKey {
    /// Reads the key, or makes and stores one when there is none.
    ///
    /// # Errors
    ///
    /// A key file that is not private or not 32 hex-encoded bytes, or one
    /// that cannot be written.
    pub(crate) fn load_or_create(paths: &PlakPaths) -> CliResult<Self> {
        if let Some(key) = Self::load(paths)? {
            return Ok(key);
        }
        let mut bytes = Zeroizing::new([0_u8; 32]);
        random(bytes.as_mut())?;
        let mut hex = Zeroizing::new(String::with_capacity(64));
        for byte in bytes.iter() {
            let _ = write!(hex, "{byte:02x}");
        }
        // Two first starts race to create the key; the first one's wins, and
        // the other reads it back rather than keeping a key nobody shares.
        let created = crate::home::secrets::create_shelf_secret(
            paths,
            SecretShelf::Web,
            COOKIE_KEY_NAME,
            &hex,
        )
        .map_err(CliFailure::new)?;
        if created {
            return Ok(Self::from_bytes(&bytes));
        }
        // A key file is there but read as none (empty or blank): it is
        // never replaced, so the operator decides.
        Self::load(paths)?.ok_or_else(|| {
            CliFailure::new(format!(
                "plak web: {} holds no cookie key; remove it to make a new one (every web client is logged out)",
                paths
                    .secret_shelf_file(SecretShelf::Web, COOKIE_KEY_NAME)
                    .display()
            ))
        })
    }

    /// The stored key, `None` when the shelf holds none.
    fn load(paths: &PlakPaths) -> CliResult<Option<Self>> {
        let stored =
            crate::home::secrets::read_shelf_secret(paths, SecretShelf::Web, COOKIE_KEY_NAME)
                .map_err(CliFailure::new)?;
        stored
            .map(|stored| {
                decode_hex(stored.expose_secret())
                    .map(|bytes| Self::from_bytes(&bytes))
                    .ok_or_else(|| {
                        CliFailure::new("plak web: the cookie key is not 32 hex-encoded bytes")
                    })
            })
            .transpose()
    }

    fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self(hmac::Key::new(hmac::HMAC_SHA256, bytes))
    }

    /// A cookie value for `login` at `origin`, valid until `expires` (unix
    /// seconds).
    fn issue(&self, origin: &str, login: &str, expires: u64) -> CliResult<String> {
        let mut nonce = [0_u8; 16];
        random(&mut nonce)?;
        let body = format!(
            "{}.{expires}.{}",
            URL_SAFE_NO_PAD.encode(login),
            URL_SAFE_NO_PAD.encode(nonce)
        );
        let tag = hmac::sign(&self.0, signed(origin, &body).as_bytes());
        Ok(format!("{body}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref())))
    }

    /// Whether `value` is a cookie this key issued to `login` at `origin`
    /// that has not expired at `now`.
    fn verify(&self, origin: &str, value: &str, login: &str, now: u64) -> bool {
        let Some((body, tag)) = value.rsplit_once('.') else {
            return false;
        };
        let Ok(tag) = URL_SAFE_NO_PAD.decode(tag) else {
            return false;
        };
        if hmac::verify(&self.0, signed(origin, body).as_bytes(), &tag).is_err() {
            return false;
        }
        let mut parts = body.split('.');
        let (Some(named), Some(expires), Some(_nonce), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        URL_SAFE_NO_PAD.decode(named).ok().as_deref() == Some(login.as_bytes())
            && expires.parse::<u64>().is_ok_and(|expires| now < expires)
    }
}

/// What a cookie's tag covers: its body under this format's label, for one
/// served origin, so a cookie minted for one origin or port of a home never
/// holds at another.
fn signed(origin: &str, body: &str) -> String {
    format!("plak-web-cookie/1|{origin}|{body}")
}

fn random(bytes: &mut [u8]) -> CliResult<()> {
    SystemRandom::new()
        .fill(bytes)
        .map_err(|_| CliFailure::new("plak web: no randomness"))
}

fn decode_hex(hex: &str) -> Option<Zeroizing<[u8; 32]>> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut key = Zeroizing::new([0_u8; 32]);
    for (index, pair) in bytes.chunks(2).enumerate() {
        key[index] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(key)
}

/// The shared state of the gate: who may connect, the key, and the limits.
pub(crate) struct Gate {
    access: WebAccess,
    key: CookieKey,
    limits: Mutex<Limits>,
}

#[derive(Default)]
struct Limits {
    connections: BTreeMap<String, usize>,
    upgrades: BTreeMap<String, Vec<Instant>>,
    refusals: Vec<Instant>,
    unlogged: u64,
}

/// Why a request was refused; the log's words, never the client's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    Method,
    Identity,
    NotAllowed,
    Host,
    Source,
    Origin,
    Cookie,
    Upgrade,
    ConnectionLimit,
    UpgradeRate,
}

impl Refusal {
    const fn reason(self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::Identity => "no tailnet identity",
            Self::NotAllowed => "login not allowed",
            Self::Host => "host",
            Self::Source => "source is this machine",
            Self::Origin => "origin",
            Self::Cookie => "session cookie",
            Self::Upgrade => "not a websocket upgrade",
            Self::ConnectionLimit => "connection limit",
            Self::UpgradeRate => "upgrade rate",
        }
    }
}

/// One connection's hold on its login's connection count.
pub(crate) struct ConnectionSlot {
    served: Arc<Served>,
    login: String,
}

impl ConnectionSlot {
    /// The tailnet login (or dev login) the connection was admitted as.
    pub(crate) fn login(&self) -> &str {
        &self.login
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut limits = self.served.gate.lock();
        if let Some(held) = limits.connections.get_mut(&self.login) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                limits.connections.remove(&self.login);
            }
        }
    }
}

impl Gate {
    /// The gate for `access`, signing with `key`. Dev mode says so.
    pub(crate) fn new(access: WebAccess, key: CookieKey) -> Self {
        if let WebAccess::Dev { login, .. } = &access {
            tracing::warn!(target: "plak::web", login = %login, "plak web in dev mode: every local connection is this login");
            eprintln!(
                "plak web: dev mode, every local connection is {login}; never expose this listener"
            );
        }
        Self {
            access,
            key,
            limits: Mutex::new(Limits::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Limits> {
        self.limits.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The login a request comes from, under this gate's identity rule.
    /// The login a request comes from, or the refusal and the login it
    /// presented (bounded, printable) when that login is not allowed.
    fn identify(&self, headers: &HeaderMap) -> Result<String, (Refusal, Option<String>)> {
        match &self.access {
            WebAccess::Dev { login, .. } => Ok(login.clone()),
            WebAccess::Tailnet { allow, .. } => {
                let mut values = headers.get_all(IDENTITY).iter();
                let (Some(value), None) = (values.next(), values.next()) else {
                    return Err((Refusal::Identity, None));
                };
                let login = value.to_str().map_err(|_| (Refusal::Identity, None))?;
                if login.is_empty() || login.len() > 256 {
                    return Err((Refusal::Identity, None));
                }
                if allow.iter().any(|allowed| allowed == login) {
                    Ok(login.to_owned())
                } else {
                    Err((Refusal::NotAllowed, Some(log_path(login))))
                }
            }
        }
    }

    /// The request's target. Behind Serve (tailnet) it is Serve's forwarded
    /// shape for the served authority, from another machine; in dev mode,
    /// the exact `Host`.
    fn check_host(&self, headers: &HeaderMap) -> Result<(), Refusal> {
        match &self.access {
            WebAccess::Tailnet { origin, .. } => {
                super::forwarded::check_serve_request(headers, origin.authority())
            }
            WebAccess::Dev { origin, .. } => {
                let mut values = headers.get_all(header::HOST).iter();
                match (values.next(), values.next()) {
                    (Some(host), None) if host.as_bytes() == origin.authority().as_bytes() => {
                        Ok(())
                    }
                    _ => Err(Refusal::Host),
                }
            }
        }
    }

    fn check_origin(&self, headers: &HeaderMap) -> Result<(), Refusal> {
        let mut values = headers.get_all(header::ORIGIN).iter();
        match (values.next(), values.next()) {
            (Some(origin), None)
                if origin.as_bytes() == self.access.origin().as_str().as_bytes() =>
            {
                Ok(())
            }
            _ => Err(Refusal::Origin),
        }
    }

    fn cookie_holds(&self, headers: &HeaderMap, login: &str) -> bool {
        let now = unix_now();
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(name, value)| {
                name == COOKIE
                    && self
                        .key
                        .verify(self.access.origin().as_str(), value, login, now)
            })
    }

    /// A `Set-Cookie` value for a fresh session cookie for `login`.
    fn set_cookie(&self, login: &str) -> CliResult<HeaderValue> {
        let expires = unix_now().saturating_add(COOKIE_LIFETIME.as_secs());
        let value = self
            .key
            .issue(self.access.origin().as_str(), login, expires)?;
        HeaderValue::from_str(&format!(
            "{COOKIE}={value}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={}",
            COOKIE_LIFETIME.as_secs()
        ))
        .map_err(|_| CliFailure::new("plak web: could not encode a cookie"))
    }

    /// Takes a connection slot for `login` and counts its upgrade.
    fn admit_connection(
        &self,
        served: &Arc<Served>,
        login: &str,
    ) -> Result<ConnectionSlot, Refusal> {
        let now = Instant::now();
        let mut limits = self.lock();
        let upgrades = limits.upgrades.entry(login.to_owned()).or_default();
        upgrades.retain(|at| now.duration_since(*at) < MINUTE);
        if upgrades.len() >= MAX_UPGRADES_PER_MINUTE {
            return Err(Refusal::UpgradeRate);
        }
        upgrades.push(now);
        let held = limits.connections.entry(login.to_owned()).or_default();
        if *held >= MAX_CONNECTIONS_PER_LOGIN {
            return Err(Refusal::ConnectionLimit);
        }
        *held += 1;
        Ok(ConnectionSlot {
            served: Arc::clone(served),
            login: login.to_owned(),
        })
    }

    /// Logs a refusal without anything secret, bounded per minute.
    fn log_refusal(&self, refusal: Refusal, path: &str, login: Option<&str>) {
        let now = Instant::now();
        let mut limits = self.lock();
        limits
            .refusals
            .retain(|at| now.duration_since(*at) < MINUTE);
        if limits.refusals.len() >= MAX_LOGGED_REFUSALS_PER_MINUTE {
            limits.unlogged = limits.unlogged.saturating_add(1);
            return;
        }
        limits.refusals.push(now);
        let unlogged = std::mem::take(&mut limits.unlogged);
        drop(limits);
        tracing::warn!(
            target: "plak::web",
            reason = refusal.reason(),
            path = %log_path(path),
            login = login.unwrap_or("-"),
            unlogged,
            "plak web refused a request"
        );
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A path for the log: printable ASCII, bounded, no query.
fn log_path(path: &str) -> String {
    path.chars()
        .take(128)
        .map(|character| {
            if character.is_ascii_graphic() {
                character
            } else {
                '?'
            }
        })
        .collect()
}

/// Answers one request.
pub(crate) fn handle(request: Request<Incoming>, served: Arc<Served>) -> Response<Full<Bytes>> {
    let path = request.uri().path().to_owned();
    match route(request, &served) {
        Ok(response) => response,
        Err((refusal, login)) => {
            served.gate.log_refusal(refusal, &path, login.as_deref());
            forbidden(refusal == Refusal::Method)
        }
    }
}

type Routed = Result<Response<Full<Bytes>>, (Refusal, Option<String>)>;

fn route(mut request: Request<Incoming>, served: &Arc<Served>) -> Routed {
    let gate = &served.gate;
    let wire = request.uri().path() == "/wire";
    // `/wire` is an upgrade, and an upgrade is a GET (RFC 6455 §4.2.1).
    if request.method() != Method::GET && (wire || request.method() != Method::HEAD) {
        return Err((Refusal::Method, None));
    }
    let login = gate.identify(request.headers())?;
    let refused = |refusal| (refusal, Some(login.clone()));
    // An absolute-form target names its own authority (RFC 9112 §3.2.2).
    if request.uri().authority().is_some() {
        return Err(refused(Refusal::Host));
    }
    gate.check_host(request.headers()).map_err(refused)?;
    if wire {
        gate.check_origin(request.headers()).map_err(refused)?;
        if !gate.cookie_holds(request.headers(), &login) {
            return Err(refused(Refusal::Cookie));
        }
        let accept = super::socket::accept_key(request.headers())
            .ok_or_else(|| refused(Refusal::Upgrade))?;
        let slot = gate.admit_connection(served, &login).map_err(refused)?;
        let upgrade = hyper::upgrade::on(&mut request);
        let session = Arc::clone(served);
        tracing::info!(target: "plak::web", login = %login, "plak web connection opened");
        tokio::spawn(async move {
            if let Ok(upgraded) = upgrade.await {
                super::socket::serve(upgraded, session, slot).await;
            }
        });
        return Ok(super::socket::switching_protocols(&accept));
    }
    let mut response = super::ui::respond(served.ui.as_ref(), &request);
    if !gate.cookie_holds(request.headers(), &login) {
        if let Ok(cookie) = gate.set_cookie(&login) {
            tracing::info!(target: "plak::web", login = %login, "plak web login");
            response.headers_mut().append(header::SET_COOKIE, cookie);
        }
    }
    Ok(response)
}

/// The one refusal every failed check gets.
fn forbidden(method: bool) -> Response<Full<Bytes>> {
    let status = if method {
        StatusCode::METHOD_NOT_ALLOWED
    } else {
        StatusCode::FORBIDDEN
    };
    let mut response = Response::new(Full::new(Bytes::from_static(b"forbidden\n")));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    super::ui::security_headers(headers, None);
    response
}

#[cfg(test)]
pub(super) fn test_key(byte: u8) -> CookieKey {
    CookieKey::from_bytes(&[byte; 32])
}

#[cfg(test)]
pub(super) fn issue_for_test(key: &CookieKey, origin: &str, login: &str, expires: u64) -> String {
    key.issue(origin, login, expires).unwrap_or_default()
}

#[cfg(test)]
pub(super) fn verify_for_test(
    key: &CookieKey,
    origin: &str,
    value: &str,
    login: &str,
    now: u64,
) -> bool {
    key.verify(origin, value, login, now)
}
