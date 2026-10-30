use super::*;

#[test]
fn present_authorize_url_has_bare_url_line() {
    let mut buf = Vec::new();
    present_authorize_url(
        &mut buf,
        "https://auth.example/oauth?x=1&y=2",
        "codex",
        false,
    )
    .unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(text.contains("Open this URL"));
    assert!(text.contains("mouse: drag the URL line only"));
    assert!(!text.contains("press c to copy"));
    assert!(
        text.lines()
            .any(|l| l == "https://auth.example/oauth?x=1&y=2")
    );
    assert!(!text.contains("│https"));
}

#[test]
fn present_authorize_url_remote_uses_cooked_recopy_hint() {
    let mut buf = Vec::new();
    present_authorize_url(
        &mut buf,
        "https://auth.example/oauth?remote=1",
        "grok",
        true,
    )
    .unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(text.contains("type c then Enter"));
    assert!(!text.contains("press c to copy"));
    assert!(
        text.lines()
            .any(|l| l == "https://auth.example/oauth?remote=1")
    );
}

#[test]
fn device_code_prompt_shows_url_code_expiry_and_anti_phishing_rule() {
    let mut buf = Vec::new();
    write_device_code_prompt(
        &mut buf,
        "codex",
        "https://auth.example/codex/device",
        "ABCD-1234",
        Duration::from_secs(899),
    )
    .unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(text.contains("https://auth.example/codex/device"));
    assert!(text.contains("ABCD-1234"));
    assert!(text.contains("expires in 15 minutes"));
    assert!(text.contains(
        "Continue only if you started this login in plak; if someone gave you this code, cancel."
    ));
    assert!(text.contains("Ctrl-C cancels"));
    assert!(!text.to_ascii_lowercase().contains("paste"));
}

#[test]
fn ssh_session_from_env_detects_any_marker() {
    use std::ffi::OsStr;
    assert!(!ssh_session_from_env(None, None, None));
    assert!(!ssh_session_from_env(
        Some(OsStr::new("")),
        Some(OsStr::new("")),
        Some(OsStr::new("")),
    ));
    assert!(ssh_session_from_env(
        Some(OsStr::new("10.0.0.1 123 10.0.0.2 22")),
        None,
        None,
    ));
    assert!(ssh_session_from_env(
        None,
        Some(OsStr::new("1.2.3.4 1 2")),
        None
    ));
    assert!(ssh_session_from_env(
        None,
        None,
        Some(OsStr::new("/dev/pts/0"))
    ));
}

#[test]
fn ssh_refusal_is_explicit_not_auto_remote() {
    let m = ssh_local_login_refusal_message(plakous::subscriptions::NativeProvider::Codex);
    assert!(m.contains("SSH session detected"));
    assert!(m.contains("plak login codex --remote"));
    assert!(!m.to_ascii_lowercase().contains("auto"));
    assert!(!m.contains("switching"));
}

#[test]
fn loopback_wait_target_decodes_redirect_uri() {
    let grok = "https://auth.x.ai/oauth2/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A56121%2Fcallback&scope=openid";
    assert_eq!(
        loopback_wait_target(grok),
        "http://127.0.0.1:56121/callback"
    );
    let codex = "https://auth.openai.com/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback";
    assert_eq!(
        loopback_wait_target(codex),
        "http://localhost:1455/auth/callback"
    );
    assert_eq!(
        loopback_wait_target("https://example/authorize"),
        "the loopback callback in the authorize URL"
    );
}

#[test]
fn local_wait_progress_says_do_not_paste() {
    let mut buf = Vec::new();
    write_local_wait_progress(
        &mut buf,
        "codex",
        true,
        "http://localhost:1455/auth/callback",
    )
    .unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(text.contains("http://localhost:1455/auth/callback"));
    assert!(text.contains("Do not paste the localhost callback URL"));
    assert!(text.contains("Ctrl-C cancels"));
    assert!(!text.contains("press c"));
}
