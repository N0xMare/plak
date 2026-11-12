use hyper::header::{HeaderMap, HeaderValue};
use std::sync::Arc;

use super::auth::{issue_for_test, test_key, verify_for_test};
use super::listen::Listen;
use super::socket::accept_key;
use super::ui::{Ui, respond};

const ORIGIN: &str = "https://plak.example.ts.net";

/// A cookie verifies for the login and origin it was issued to, until it
/// expires, and under the key that signed it only.
#[test]
fn a_cookie_is_bound_to_its_login_origin_key_and_lifetime() {
    let key = test_key(7);
    let cookie = issue_for_test(&key, ORIGIN, "a@example.com", 100);
    assert!(verify_for_test(&key, ORIGIN, &cookie, "a@example.com", 99));
    assert!(!verify_for_test(
        &key,
        ORIGIN,
        &cookie,
        "a@example.com",
        100
    ));
    assert!(!verify_for_test(&key, ORIGIN, &cookie, "b@example.com", 99));
    assert!(!verify_for_test(
        &test_key(8),
        ORIGIN,
        &cookie,
        "a@example.com",
        99
    ));
    let (body, _) = cookie.rsplit_once('.').unwrap();
    assert!(!verify_for_test(
        &key,
        ORIGIN,
        &format!("{body}.AAAA"),
        "a@example.com",
        99
    ));
    assert!(!verify_for_test(&key, ORIGIN, "", "a@example.com", 99));
    assert!(!verify_for_test(
        &key,
        "https://other.example.ts.net",
        &cookie,
        "a@example.com",
        99
    ));
}

/// Two cookies for one login differ: each login gets a fresh one.
#[test]
fn every_issued_cookie_is_fresh() {
    let key = test_key(7);
    assert_ne!(
        issue_for_test(&key, ORIGIN, "a@example.com", 100),
        issue_for_test(&key, ORIGIN, "a@example.com", 100)
    );
}

/// The RFC 6455 example handshake, and a request that is not an upgrade.
#[test]
fn the_accept_key_is_rfc_6455s() {
    let mut headers = HeaderMap::new();
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    headers.insert(
        "connection",
        HeaderValue::from_static("keep-alive, Upgrade"),
    );
    headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
    headers.insert(
        "sec-websocket-key",
        HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
    );
    assert_eq!(
        accept_key(&headers).as_deref(),
        Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
    );
    headers.insert("sec-websocket-version", HeaderValue::from_static("8"));
    assert_eq!(accept_key(&headers), None);
}

/// A listener is a unix socket path or loopback; nothing else binds.
#[test]
fn a_listener_is_private() {
    assert!(matches!(
        Listen::parse("unix:/run/plak/web.sock"),
        Ok(Listen::Unix(_))
    ));
    assert!(matches!(
        Listen::parse("127.0.0.1:8123"),
        Ok(Listen::Loopback(_))
    ));
    for refused in [
        "unix:relative.sock",
        "0.0.0.0:8123",
        "192.168.1.2:80",
        "[::1]:80",
    ] {
        assert!(Listen::parse(refused).is_err(), "{refused}");
    }
}

fn ui_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("csp.json"),
        r#"{"header": "default-src 'self'; frame-ancestors 'none'", "report_only_header": "script-src 'none'"}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("index.html"), "<!doctype html>app").unwrap();
    std::fs::write(dir.path().join("service-worker.js"), "worker").unwrap();
    std::fs::create_dir_all(dir.path().join("_app/immutable")).unwrap();
    std::fs::write(dir.path().join("_app/immutable/a.js"), "asset").unwrap();
    std::fs::write(dir.path().join(".secret"), "not served").unwrap();
    dir
}

#[test]
fn captured_console_switches_gate_bind_and_survive_reload() {
    use plakd_wire_types::console::ConfiguredSwitches;
    let dir = ui_dir();
    let legacy = std::fs::read_to_string(dir.path().join("csp.json")).unwrap();
    let strict = "default-src 'self'; connect-src 'self'; require-trusted-types-for 'script'; frame-ancestors 'none'; object-src 'none'; base-uri 'none'";
    for (accounts, config) in [(false, false), (true, false), (false, true)] {
        let switches = ConfiguredSwitches {
            web_accounts_read: accounts,
            web_config_read: config,
            web_credentials_write: false,
            web_config_write: false,
            web_files_read: false,
        };
        std::fs::write(dir.path().join("csp.json"), &legacy).unwrap();
        assert_eq!(
            super::load_ui(Some(dir.path()), switches).is_err(),
            accounts || config
        );
        std::fs::write(
            dir.path().join("csp.json"),
            serde_json::json!({"header":strict}).to_string(),
        )
        .unwrap();
        let builds = super::load_ui(Some(dir.path()), switches).unwrap().unwrap();
        let current = builds.snapshot();
        std::fs::write(dir.path().join("csp.json"), &legacy).unwrap();
        assert_eq!(builds.reload().is_err(), accounts || config);
        assert_eq!(
            Arc::ptr_eq(&current, &builds.snapshot()),
            accounts || config
        );
    }
}

#[test]
fn console_csp_is_enforced_on_cold_load_and_every_reload_without_replacing_a_good_build() {
    let dir = ui_dir();
    assert!(
        Ui::load(dir.path(), true).is_err(),
        "legacy CSP is not sufficient for console data"
    );
    let policy = "default-src 'self'; connect-src 'self'; require-trusted-types-for 'script'; frame-ancestors 'none'; object-src 'none'; base-uri 'none'";
    std::fs::write(
        dir.path().join("csp.json"),
        serde_json::json!({"header":policy}).to_string(),
    )
    .unwrap();
    let builds = super::ui::Builds::load(dir.path(), true).unwrap();
    let current = builds.snapshot();
    let weak = "default-src 'self'; frame-ancestors 'none'";
    std::fs::write(
        dir.path().join("csp.json"),
        serde_json::json!({"header":weak,"report_only_header":policy}).to_string(),
    )
    .unwrap();
    assert!(
        builds.reload().is_err(),
        "report-only cannot replace enforced policy"
    );
    assert!(Arc::ptr_eq(&current, &builds.snapshot()));
    assert_eq!(
        header(
            &get(&builds.snapshot(), "/", &[]),
            "content-security-policy"
        ),
        Some(policy)
    );
}

#[tokio::test]
async fn ui_reload_swaps_files_and_policy_together_and_keeps_old_build_on_failure() {
    use http_body_util::BodyExt as _;
    let dir = ui_dir();
    let builds = super::ui::Builds::load(dir.path(), false).unwrap();
    let old = builds.snapshot();
    let policy = "default-src 'none'; frame-ancestors 'none'";
    std::fs::write(dir.path().join("index.html"), "new build").unwrap();
    std::fs::write(
        dir.path().join("csp.json"),
        serde_json::json!({"header": policy}).to_string(),
    )
    .unwrap();
    builds.reload().unwrap();
    let current = builds.snapshot();
    assert!(!Arc::ptr_eq(&old, &current));
    let response = get(&current, "/", &[]);
    assert_eq!(header(&response, "content-security-policy"), Some(policy));
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "new build"
    );
    assert_eq!(
        get(&old, "/", &[])
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
        "<!doctype html>app"
    );

    for invalid in [
        "not json",
        r#"{"header":"frame-ancestors *"}"#,
        r#"{"header":"frame-ancestors 'none'","unexpected":true}"#,
    ] {
        std::fs::write(dir.path().join("csp.json"), invalid).unwrap();
        assert!(Ui::load(dir.path(), false).is_err());
        assert!(builds.reload().is_err());
        assert!(Arc::ptr_eq(&current, &builds.snapshot()));
    }
    std::fs::write(
        dir.path().join("csp.json"),
        serde_json::json!({"header": policy}).to_string(),
    )
    .unwrap();
    let huge = std::fs::File::create(dir.path().join("large.js")).unwrap();
    huge.set_len(32 * 1024 * 1024 + 1).unwrap();
    assert!(Ui::load(dir.path(), false).is_err());
    assert!(builds.reload().is_err());
    assert!(Arc::ptr_eq(&current, &builds.snapshot()));
}

fn get(
    ui: &Ui,
    path: &str,
    headers: &[(&'static str, &'static str)],
) -> hyper::Response<http_body_util::Full<bytes::Bytes>> {
    let mut request = hyper::Request::get(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    respond(Some(ui), &request.body(()).unwrap())
}

fn header<'a>(
    response: &'a hyper::Response<http_body_util::Full<bytes::Bytes>>,
    name: &str,
) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

/// Immutable assets cache for a year, everything else revalidates; every
/// answer carries the build's CSP and the isolation headers; an app route
/// falls back to index.html but a missing asset does not.
#[test]
fn static_files_carry_their_cache_rules_and_security_headers() {
    let dir = ui_dir();
    let ui = Ui::load(dir.path(), false).unwrap();
    let asset = get(&ui, "/_app/immutable/a.js", &[]);
    assert_eq!(
        header(&asset, "cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    for path in ["/", "/index.html", "/service-worker.js", "/sessions/abc"] {
        let response = get(&ui, path, &[]);
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(
            header(&response, "cache-control"),
            Some("no-cache"),
            "{path}"
        );
    }
    let fallback = get(&ui, "/sessions/abc", &[]);
    assert_eq!(
        header(&fallback, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(get(&ui, "/_app/immutable/gone.js", &[]).status(), 404);
    for name in [
        "content-security-policy",
        "content-security-policy-report-only",
        "x-content-type-options",
        "referrer-policy",
        "cross-origin-opener-policy",
        "cross-origin-resource-policy",
        "permissions-policy",
    ] {
        assert!(header(&asset, name).is_some(), "{name}");
    }
    assert_eq!(header(&asset, "x-frame-options"), Some("DENY"));
    assert_eq!(
        header(&asset, "content-security-policy"),
        Some("default-src 'self'; frame-ancestors 'none'")
    );
    // Only the build's listed files are served: a dotfile is not one.
    let dotfile = get(&ui, "/.secret", &[]);
    assert_eq!(
        header(&dotfile, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert!(
        asset
            .headers()
            .keys()
            .all(|name| !name.as_str().starts_with("access-control-"))
    );
}

/// A worker registered from any path but /service-worker.js gets the kill
/// switch and Clear-Site-Data; the real worker is served from the build.
#[test]
fn a_stray_service_worker_gets_the_kill_switch() {
    let dir = ui_dir();
    let ui = Ui::load(dir.path(), false).unwrap();
    let stray = get(&ui, "/sw.js", &[("service-worker", "script")]);
    assert_eq!(
        header(&stray, "clear-site-data"),
        Some("\"cache\", \"storage\"")
    );
    assert_eq!(
        header(&stray, "content-type"),
        Some("text/javascript; charset=utf-8")
    );
    let real = get(&ui, "/service-worker.js", &[("service-worker", "script")]);
    assert_eq!(header(&real, "clear-site-data"), None);
    // A build with no worker answers even /service-worker.js with the
    // switch, so a worker planted there cannot outlive the host's repair.
    std::fs::remove_file(dir.path().join("service-worker.js")).unwrap();
    let ui = Ui::load(dir.path(), false).unwrap();
    let absent = get(&ui, "/service-worker.js", &[("service-worker", "script")]);
    assert_eq!(
        header(&absent, "clear-site-data"),
        Some("\"cache\", \"storage\"")
    );
}

/// A build whose CSP lets it be framed is refused at start.
#[test]
fn a_ui_that_allows_framing_is_refused() {
    let dir = ui_dir();
    for header in [
        "default-src 'self'",
        "default-src 'self'; frame-ancestors 'self'",
        "default-src 'self'; frame-ancestors 'none' https://x.example",
    ] {
        std::fs::write(
            dir.path().join("csp.json"),
            serde_json::json!({ "header": header }).to_string(),
        )
        .unwrap();
        assert!(Ui::load(dir.path(), false).is_err(), "{header}");
    }
}

/// Serve's own shape is admitted, from another machine only; anything a
/// client could send instead is not.
#[test]
fn only_serves_forwarded_shape_from_another_machine_is_admitted() {
    use super::forwarded::check_serve_request;
    let shape = |host: &str, forwarded: Option<&str>, proto: Option<&str>, source: Option<&str>| {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_str(host).unwrap());
        for (name, value) in [
            ("x-forwarded-host", forwarded),
            ("x-forwarded-proto", proto),
            ("x-forwarded-for", source),
        ] {
            if let Some(value) = value {
                headers.insert(name, HeaderValue::from_str(value).unwrap());
            }
        }
        check_serve_request(&headers, "plak.example.ts.net")
    };
    let serve = |source: &str| {
        shape(
            "localhost",
            Some("plak.example.ts.net"),
            Some("https"),
            Some(source),
        )
    };
    assert!(serve("192.0.2.10").is_ok());
    assert!(serve("2001:db8::10").is_ok());
    for local in ["127.0.0.1", "::1", "0.0.0.0", "127.0.0.2"] {
        assert!(serve(local).is_err(), "{local}");
    }
    // This machine's own routed address (as Serve forwards a connection
    // from the serving node itself) is refused too, when it has one.
    let own = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| socket.connect("192.0.2.1:9").map(|()| socket))
        .and_then(|socket| socket.local_addr());
    if let Ok(own) = own {
        let address = own.ip().to_string();
        assert!(serve(&address).is_err(), "{own}");
    }
    assert!(serve("192.0.2.10, 192.0.2.11").is_err());
    // The shape a client would send directly, which Serve never forwards.
    assert!(shape("plak.example.ts.net", None, None, Some("192.0.2.10")).is_err());
    assert!(shape("localhost", None, Some("https"), Some("192.0.2.10")).is_err());
    assert!(
        shape(
            "localhost",
            Some("evil.example"),
            Some("https"),
            Some("192.0.2.10")
        )
        .is_err()
    );
    assert!(
        shape(
            "localhost",
            Some("plak.example.ts.net"),
            Some("http"),
            Some("192.0.2.10")
        )
        .is_err()
    );
    assert!(
        shape(
            "localhost",
            Some("plak.example.ts.net"),
            Some("https"),
            None
        )
        .is_err()
    );
}

/// A UI directory without its CSP is refused at start.
#[test]
fn a_ui_without_csp_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "app").unwrap();
    assert!(Ui::load(dir.path(), false).is_err());
}

/// A cookie-key file that holds no key is never replaced, and the start
/// fails naming it, rather than retrying forever.
#[test]
fn a_blank_cookie_key_file_fails_once_and_names_itself() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home);
    let shelf = crate::home::paths::SecretShelf::Web;
    paths.ensure_secret_shelf_dir(shelf).unwrap();
    let file = paths.secret_shelf_file(shelf, "cookie-key");
    for blank in ["", "   \n"] {
        std::fs::write(&file, blank).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let Err(failure) = super::auth::CookieKey::load_or_create(&paths) else {
            panic!("a blank key file was accepted");
        };
        assert!(
            failure.message().contains(&file.display().to_string()),
            "{}",
            failure.message()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), blank);
    }
}
