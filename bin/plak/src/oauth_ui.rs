//! Operator-facing OAuth presentation: clean URL panel, clipboard copy, login waits.

use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

/// Soft warning while still waiting for local loopback (product UX).
pub const LOCAL_SOFT_WARN: Duration = Duration::from_secs(30);
/// Product hard timeout for local browser login (library deadline is longer).
pub const LOCAL_HARD_TIMEOUT: Duration = Duration::from_secs(120);

/// Present an authorize URL for keyboard + mouse friendly copy.
///
/// Layout: short instruction, top/bottom rules on **separate** lines, bare URL
/// alone (no side borders so mouse drag does not pick frame glyphs).
/// Attempts clipboard copy once immediately. Local loopback wait never puts
/// the TTY in raw mode (that swallowed Ctrl-C and made paste look like a hang).
pub fn present_authorize_url(
    out: &mut dyn Write,
    url: &str,
    account: &str,
    remote: bool,
) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "Open this URL in a browser to sign in to {account}:")?;
    writeln!(out)?;
    if remote {
        writeln!(
            out,
            "  re-copy: type c then Enter at the prompt  ·  mouse: drag the URL line only"
        )?;
    } else {
        writeln!(out, "  mouse: drag the URL line only")?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "────────────────────────────────────────────────────────"
    )?;
    // Bare URL on its own line — no side borders (clean mouse select / triple-click).
    writeln!(out, "{url}")?;
    writeln!(
        out,
        "────────────────────────────────────────────────────────"
    )?;
    writeln!(out)?;
    match try_copy_to_clipboard(url) {
        CopyResult::Copied => {
            writeln!(out, "URL copied to clipboard.")?;
        }
        CopyResult::Unavailable => {
            if remote {
                writeln!(
                    out,
                    "Clipboard unavailable — select the URL line above, or type c then Enter at the prompt."
                )?;
            } else {
                writeln!(out, "Clipboard unavailable — select the URL line above.")?;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyResult {
    Copied,
    Unavailable,
}

/// Best-effort clipboard: OSC 52 (when stderr is a TTY), then platform tools.
#[must_use]
pub fn try_copy_to_clipboard(text: &str) -> CopyResult {
    if try_osc52_copy(text) {
        return CopyResult::Copied;
    }
    if try_external_clipboard(text) {
        return CopyResult::Copied;
    }
    CopyResult::Unavailable
}

fn try_osc52_copy(text: &str) -> bool {
    // OSC 52: ESC ] 52 ; c ; <base64> BEL  — many terminals support this over SSH.
    if !rustix::termios::isatty(io::stderr()) {
        return false;
    }
    let encoded = B64.encode(text.as_bytes());
    if encoded.len() > 12 * 1024 {
        return false;
    }
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let mut err = io::stderr().lock();
    err.write_all(seq.as_bytes()).is_ok() && err.flush().is_ok()
}

fn try_external_clipboard(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        if pipe_to(&["pbcopy"], text) {
            return true;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if pipe_to(&["wl-copy"], text) {
            return true;
        }
        if pipe_to(&["xclip", "-selection", "clipboard"], text) {
            return true;
        }
        if pipe_to(&["xsel", "--clipboard", "--input"], text) {
            return true;
        }
    }
    #[cfg(windows)]
    {
        if pipe_to(&["clip"], text) {
            return true;
        }
    }
    let _ = text;
    false
}

fn pipe_to(cmd: &[&str], text: &str) -> bool {
    let Some((prog, rest)) = cmd.split_first() else {
        return false;
    };
    let Ok(mut child) = Command::new(prog)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        return false;
    };
    if stdin.write_all(text.as_bytes()).is_err() {
        let _ = child.kill();
        return false;
    }
    drop(stdin);
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// Decode the `redirect_uri` query value from an authorize URL, when present.
#[must_use]
pub fn loopback_wait_target(authorize_url: &str) -> String {
    const KEY: &str = "redirect_uri=";
    let Some(start) = authorize_url.find(KEY) else {
        return "the loopback callback in the authorize URL".to_owned();
    };
    let rest = &authorize_url[start + KEY.len()..];
    let encoded = rest.split('&').next().unwrap_or(rest);
    percent_decode(encoded)
}

fn percent_decode(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(hi) = from_hex(bytes[i + 1])
            && let Some(lo) = from_hex(bytes[i + 2])
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Account label for login CLI (`codex` / `grok` / `claude`).
#[must_use]
pub fn account_cli_name(provider: plakous::subscriptions::NativeProvider) -> &'static str {
    match provider {
        plakous::subscriptions::NativeProvider::Codex => "codex",
        plakous::subscriptions::NativeProvider::Xai => "grok",
        plakous::subscriptions::NativeProvider::Claude => "claude",
        _ => provider.as_str(),
    }
}

/// True when the process environment indicates an SSH session.
///
/// Used only for an **explicit** refuse-local-login path (not silent mode switch).
/// Any of `SSH_CONNECTION`, `SSH_CLIENT`, or `SSH_TTY` counts.
#[must_use]
pub fn ssh_session_detected() -> bool {
    ssh_session_from_env(
        std::env::var_os("SSH_CONNECTION").as_deref(),
        std::env::var_os("SSH_CLIENT").as_deref(),
        std::env::var_os("SSH_TTY").as_deref(),
    )
}

/// Pure env check for tests (non-empty values only).
#[must_use]
pub fn ssh_session_from_env(
    ssh_connection: Option<&std::ffi::OsStr>,
    ssh_client: Option<&std::ffi::OsStr>,
    ssh_tty: Option<&std::ffi::OsStr>,
) -> bool {
    [ssh_connection, ssh_client, ssh_tty]
        .into_iter()
        .flatten()
        .any(|v| !v.is_empty())
}

/// Explicit refusal when local browser login is requested under SSH.
#[must_use]
pub fn ssh_local_login_refusal_message(account: &str) -> String {
    format!(
        "SSH session detected — local browser login is not available here.\n\
         \n\
         Re-run with the explicit remote flag:\n\
         \n\
           plak login {account} --remote\n\
         \n\
         Then open the authorize URL in a browser on your machine and paste the\n\
         authorization code or callback URL when prompted."
    )
}

/// Remotely-oriented recovery lines for local-login failures.
#[must_use]
pub fn remote_login_hint(account: &str) -> String {
    format!(
        "If you are on SSH, headless, or the browser is elsewhere, run:\n\
         \n\
           plak login {account} --remote\n\
         \n\
         Then paste the authorization code or callback URL when prompted."
    )
}

/// Format the hard-timeout operator error (static-friendly for tests).
#[must_use]
pub fn local_login_timeout_message(account: &str, waited: Duration) -> String {
    format!(
        "no browser callback within {}s\n\n{}",
        waited.as_secs(),
        remote_login_hint(account)
    )
}

/// Paste-complete login instructions (Claude, Grok, Codex remote).
pub fn write_authorization_paste_instructions(
    out: &mut dyn Write,
    provider: plakous::subscriptions::NativeProvider,
    completion: plakous::subscriptions::NativeLoginCompletion,
) -> io::Result<()> {
    use plakous::subscriptions::{NativeLoginCompletion, NativeProvider};
    match completion {
        NativeLoginCompletion::RedirectUrl => {
            writeln!(
                out,
                "After authorizing in the browser, paste the COMPLETE final"
            )?;
            writeln!(
                out,
                "callback URL from the address bar (localhost may fail to load — expected)."
            )?;
            writeln!(
                out,
                "Must include ?code=…&state=… (e.g. http://localhost:1455/auth/callback?…)."
            )?;
        }
        NativeLoginCompletion::AuthorizationCodeOrRedirectUrl
            if provider == NativeProvider::Claude =>
        {
            writeln!(
                out,
                "After authorizing, the callback page shows an authorization code."
            )?;
            writeln!(out, "Paste either:")?;
            writeln!(out, "  • the authorization code, or")?;
            writeln!(
                out,
                "  • the full callback URL (https://platform.claude.com/oauth/code/callback?code=…)"
            )?;
        }
        NativeLoginCompletion::AuthorizationCodeOrRedirectUrl
            if provider == NativeProvider::Xai =>
        {
            writeln!(
                out,
                "After authorizing, the page says to copy a code into Grok Build."
            )?;
            writeln!(out, "Paste that authorization code here (then Enter).")?;
            writeln!(
                out,
                "The full callback URL is also accepted if you have it."
            )?;
        }
        NativeLoginCompletion::AuthorizationCodeOrRedirectUrl => {
            writeln!(out, "After authorizing, paste either:")?;
            writeln!(out, "  • the full callback URL from the address bar")?;
            writeln!(
                out,
                "    (http://127.0.0.1:<port>/callback?code=…&state=…), or"
            )?;
            writeln!(out, "  • the bare authorization code alone (also accepted)")?;
        }
        NativeLoginCompletion::Automatic => {}
    }
    writeln!(out, "At the prompt:")?;
    writeln!(out, "  • paste the callback URL or bare code, then Enter")?;
    writeln!(
        out,
        "  • or type  c  then Enter to re-copy the authorize URL"
    )?;
    writeln!(out, "  • empty line cancels")?;
    Ok(())
}

/// Soft progress lines while waiting for local loopback.
pub fn write_local_wait_progress(
    out: &mut dyn Write,
    account: &str,
    browser_opened: bool,
    loopback_target: &str,
) -> io::Result<()> {
    if browser_opened {
        writeln!(
            out,
            "Waiting for the browser to finish sign-in on this machine."
        )?;
    } else {
        writeln!(
            out,
            "No local browser was opened. Open the URL above on this machine,"
        )?;
        writeln!(
            out,
            "or cancel (Ctrl-C) and use:  plak login {account} --remote"
        )?;
    }
    writeln!(out, "Plak is listening for: {loopback_target}")?;
    writeln!(
        out,
        "Do not paste the localhost callback URL here — the CLI receives it automatically."
    )?;
    writeln!(
        out,
        "  reminder at {}s · timeout at {}s · Ctrl-C cancels",
        LOCAL_SOFT_WARN.as_secs(),
        LOCAL_HARD_TIMEOUT.as_secs()
    )?;
    out.flush()
}

/// Background soft-warning after [`LOCAL_SOFT_WARN`] if still waiting.
pub fn spawn_soft_wait_reminder(account: String, waiting: Arc<AtomicBool>) {
    thread::spawn(move || {
        thread::sleep(LOCAL_SOFT_WARN);
        if waiting.load(Ordering::Relaxed) {
            let _ = writeln!(
                io::stderr(),
                "plak: still waiting for the browser to finish ({account})."
            );
            let _ = writeln!(
                io::stderr(),
                "  Do not paste here. Finish in the browser, or Ctrl-C to cancel."
            );
            let _ = io::stderr().flush();
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
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
    fn grok_paste_instructions_mention_grok_build_code() {
        let mut buf = Vec::new();
        write_authorization_paste_instructions(
            &mut buf,
            plakous::subscriptions::NativeProvider::Xai,
            plakous::subscriptions::NativeLoginCompletion::AuthorizationCodeOrRedirectUrl,
        )
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Grok Build"));
        assert!(text.contains("Paste that authorization code"));
    }

    #[test]
    fn remote_hint_mentions_flag() {
        let h = remote_login_hint("codex");
        assert!(h.contains("--remote"));
        assert!(h.contains("plak login codex --remote"));
    }

    #[test]
    fn timeout_message_includes_wait_and_remote() {
        let m = local_login_timeout_message("grok", Duration::from_secs(120));
        assert!(m.contains("120"));
        assert!(m.contains("plak login grok --remote"));
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
        let m = ssh_local_login_refusal_message("codex");
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
}
