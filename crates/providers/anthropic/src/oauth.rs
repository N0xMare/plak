//! Claude Code OAuth pins, identity, window table, and token JSON.
//!
//! This is the Messages-family OAuth *route* costume, not Claude Code's product
//! loop. Login/token shapes match the documented Claude Code OAuth contract.
//! Inference pins the reviewed Claude Code 2.1.295 Messages costume (HTTP
//! attribution plus `system[0]` billing with native `cch` attestation and
//! `system[1]` gateway identity).
//! The Plak product prompt remains a later system block.

use plakous_model::ReasoningEffort;
use serde::Deserialize;

use crate::{AnthropicError, AnthropicErrorKind, AnthropicErrorPhase};

/// Public Claude Code OAuth client identifier (not a secret).
pub const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Authorize origin for inference-capable Claude Max tokens.
pub const CLAUDE_OAUTH_AUTHORIZE_ORIGIN: &str = "https://claude.com";
/// Authorize path. Console/platform authorize can mint tokens that refresh but
/// are rejected by `/v1/messages`.
pub const CLAUDE_OAUTH_AUTHORIZE_PATH: &str = "/cai/oauth/authorize";
/// Token origin.
pub const CLAUDE_OAUTH_TOKEN_ORIGIN: &str = "https://platform.claude.com";
/// Token path.
pub const CLAUDE_OAUTH_TOKEN_PATH: &str = "/v1/oauth/token";
/// Canonical HTTPS redirect registered for the public client.
pub const CLAUDE_OAUTH_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
/// Historical console callback still observed on some pasted results. Token
/// exchange always uses [`CLAUDE_OAUTH_REDIRECT_URI`].
pub const CLAUDE_OAUTH_LEGACY_REDIRECT_URI: &str =
    "https://console.anthropic.com/oauth/code/callback";
/// One enrolled Claude OAuth coding model: the OAuth catalog, the OAuth
/// context contract, the effort ladder and the OAuth Messages costume read
/// it. The Anthropic API route reads its own reviewed table (`api_models`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaudeEnrollment {
    /// Exact model literal.
    pub model: &'static str,
    /// Model family the descriptor carries.
    pub family: &'static str,
    /// Input window in tokens. The Anthropic Models API reports
    /// `max_input_tokens` = 1,000,000 for every enrolled row (2026-09-26).
    pub context_window: u64,
    /// Output ceiling (`max_output_tokens.upper` in Claude Code 2.1.295).
    /// Product output bounds, the descriptor maximum and compaction reservation
    /// use this. It is not the Messages `max_tokens` value.
    pub max_output_tokens: u32,
    /// Claude Code request default (`max_output_tokens.default`). OAuth
    /// Messages `max_tokens` is the minimum of the configured bound and this.
    pub request_max_tokens: u32,
    /// Exact OAuth `anthropic-beta` header, capture order included.
    pub oauth_betas: &'static str,
    /// Offered only on the Claude OAuth route.
    pub oauth: bool,
}

/// Registry flags that select one model's OAuth request costume.
struct RequestCostume {
    request_max_tokens: u32,
    per_turn_effort: bool,
    mid_conv_tool_change: bool,
    /// Haiku puts `claude-code-20250219` after the six-beta prefix.
    claude_code_after_prefix: bool,
}

const fn claude(
    model: &'static str,
    family: &'static str,
    oauth: bool,
    costume: RequestCostume,
) -> ClaudeEnrollment {
    ClaudeEnrollment {
        model,
        family,
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        request_max_tokens: costume.request_max_tokens,
        oauth_betas: oauth_betas(
            costume.claude_code_after_prefix,
            costume.per_turn_effort,
            costume.mid_conv_tool_change,
        ),
        oauth,
    }
}

/// Every enrolled Claude coding model, in picker order.
///
/// Every row is reviewed against its own 2.1.295 native capture (low effort);
/// betas and `max_tokens` match the earlier rows, and Fable 5.1, previously
/// carried, takes the non-Haiku 16-beta header and 64000. Haiku is the only
/// row whose id puts `claude-code-20250219` after the prefix (seventh).
pub const CLAUDE_ENROLLMENTS: [ClaudeEnrollment; 6] = [
    claude(
        "claude-sonnet-5",
        "claude-5-sonnet",
        true,
        RequestCostume {
            request_max_tokens: 64_000,
            per_turn_effort: false,
            mid_conv_tool_change: false,
            claude_code_after_prefix: false,
        },
    ),
    claude(
        "claude-sonnet-5-5",
        "claude-5-sonnet",
        true,
        RequestCostume {
            request_max_tokens: 128_000,
            per_turn_effort: true,
            mid_conv_tool_change: true,
            claude_code_after_prefix: false,
        },
    ),
    claude(
        "claude-haiku-5-5",
        "claude-5-haiku",
        true,
        RequestCostume {
            request_max_tokens: 128_000,
            per_turn_effort: true,
            mid_conv_tool_change: true,
            claude_code_after_prefix: true,
        },
    ),
    claude(
        "claude-fable-5-1",
        "claude-5-fable",
        true,
        RequestCostume {
            request_max_tokens: 64_000,
            per_turn_effort: true,
            mid_conv_tool_change: true,
            claude_code_after_prefix: false,
        },
    ),
    claude(
        "claude-opus-5",
        "claude-5-opus",
        true,
        RequestCostume {
            request_max_tokens: 64_000,
            per_turn_effort: false,
            mid_conv_tool_change: true,
            claude_code_after_prefix: false,
        },
    ),
    claude(
        "claude-opus-5-5",
        "claude-5-opus",
        true,
        RequestCostume {
            request_max_tokens: 128_000,
            per_turn_effort: true,
            mid_conv_tool_change: true,
            claude_code_after_prefix: false,
        },
    ),
];

/// The enrollment row for one exact literal model id.
#[must_use]
pub fn claude_enrollment(model: &str) -> Option<&'static ClaudeEnrollment> {
    CLAUDE_ENROLLMENTS.iter().find(|row| row.model == model)
}

/// Whether an exact literal is enrolled for Claude OAuth.
#[must_use]
pub fn claude_oauth_enrolled_model(model: &str) -> bool {
    claude_enrollment(model).is_some_and(|row| row.oauth)
}

/// Every enrolled Claude OAuth model id, in table order.
pub fn claude_oauth_enrolled_models() -> impl Iterator<Item = &'static str> {
    CLAUDE_ENROLLMENTS
        .iter()
        .filter(|row| row.oauth)
        .map(|row| row.model)
}
const CLAUDE_OAUTH_EXTENDED_REASONING_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// Exact adaptive-thinking effort values admitted for one enrolled Claude OAuth model.
///
/// Unknown and unenrolled model ids fail closed to no choices.
#[must_use]
pub fn claude_oauth_reasoning_effort_options(model: &str) -> &'static [ReasoningEffort] {
    if claude_oauth_enrolled_model(model) {
        CLAUDE_OAUTH_EXTENDED_REASONING_EFFORTS
    } else {
        &[]
    }
}
/// Exact authorize scope string in pinned wire order.
pub const CLAUDE_OAUTH_AUTHORIZE_SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// Refresh scopes omit `org:create_api_key`.
pub const CLAUDE_OAUTH_REFRESH_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// Required inference scope on an admitted token.
pub const CLAUDE_OAUTH_INFERENCE_SCOPE: &str = "user:inference";

/// Messages API origin (same host as Platform; OAuth uses Bearer + `?beta=true`).
pub const CLAUDE_OAUTH_MESSAGES_ORIGIN: &str = "https://api.anthropic.com";
/// OAuth Messages path including the required beta query.
pub const CLAUDE_OAUTH_MESSAGES_PATH: &str = "/v1/messages?beta=true";
/// Exact route identity isolated from Console API-key `anthropic.platform`.
pub const CLAUDE_OAUTH_ROUTE_ID: &str = "anthropic.claude_oauth";
/// Exact compatibility profile identity.
pub const CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID: &str =
    "anthropic.claude.oauth.compat.2026-09-22.r2";
/// Pin revision for this OAuth dialect.
pub const CLAUDE_OAUTH_SOURCE_REVISION: &str = "claude-oauth.2026-09-22.v3";
/// Operator notice revision for Claude OAuth compatibility consent.
pub const CLAUDE_OAUTH_NOTICE_REVISION: &str = "anthropic.claude.oauth.notice.2026-08-20.r1";
/// Kill-switch revision for Claude OAuth compatibility.
pub const CLAUDE_OAUTH_KILL_SWITCH_REVISION: &str = "anthropic.claude.oauth.kill.2026-08-20.r1";
/// UTC review time for the Claude OAuth pin table.
pub const CLAUDE_OAUTH_REVIEWED_AT_UNIX: u64 = 1_790_035_200;
/// Exclusive expiry for the Claude OAuth pin table.
pub const CLAUDE_OAUTH_EXPIRES_AT_UNIX: u64 = 1_821_571_200;
/// Protocol identity shared with the Messages kernel.
pub const CLAUDE_OAUTH_PROTOCOL_ID: &str = "anthropic.messages";
/// Adapter revision for Claude OAuth lowering.
pub const CLAUDE_OAUTH_ADAPTER_REVISION: &str = "anthropic.claude.oauth.2026-10-09.v8";

/// OAuth beta header for a model with neither extra flag and `claude-code` first.
///
/// This is `claude-sonnet-5`'s captured header. It is not a shared costume:
/// every enrolled row carries its own [`ClaudeEnrollment::oauth_betas`].
pub const CLAUDE_OAUTH_BASELINE_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";

const OAUTH_BETA_MID_TOOLS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,mid-conversation-tool-changes-2026-07-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
const OAUTH_BETA_PER_TURN: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,per-turn-control-2026-07-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
const OAUTH_BETA_BOTH: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
const OAUTH_BETA_HAIKU_BASELINE: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,claude-code-20250219,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
const OAUTH_BETA_HAIKU_TOOLS: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,claude-code-20250219,mid-conversation-tool-changes-2026-07-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
const OAUTH_BETA_HAIKU_PER_TURN: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,claude-code-20250219,per-turn-control-2026-07-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
const OAUTH_BETA_HAIKU_BOTH: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,claude-code-20250219,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";

/// Header order from Claude Code 2.1.295: the prefix, then `claude-code` when
/// the model id is Haiku, then `per-turn-control` for `per_turn_effort`, then
/// `mid-conversation-tool-changes` for `mid_conv_tool_change`, then the tail.
const fn oauth_betas(
    claude_code_after_prefix: bool,
    per_turn_effort: bool,
    mid_conv_tool_change: bool,
) -> &'static str {
    match (
        claude_code_after_prefix,
        per_turn_effort,
        mid_conv_tool_change,
    ) {
        (false, false, false) => CLAUDE_OAUTH_BASELINE_BETAS,
        (false, false, true) => OAUTH_BETA_MID_TOOLS,
        (false, true, false) => OAUTH_BETA_PER_TURN,
        (false, true, true) => OAUTH_BETA_BOTH,
        (true, false, false) => OAUTH_BETA_HAIKU_BASELINE,
        (true, false, true) => OAUTH_BETA_HAIKU_TOOLS,
        (true, true, false) => OAUTH_BETA_HAIKU_PER_TURN,
        (true, true, true) => OAUTH_BETA_HAIKU_BOTH,
    }
}

/// Exact OAuth `anthropic-beta` header for one model.
///
/// An unenrolled id keeps the no-extra-flag baseline. Effort does not change
/// the header: low and max captures of the same model are identical.
#[must_use]
pub fn claude_oauth_beta_header(model: &str) -> &'static str {
    claude_enrollment(model).map_or(CLAUDE_OAUTH_BASELINE_BETAS, |row| row.oauth_betas)
}

/// Claude Code's Messages `max_tokens` default for one enrolled OAuth model.
///
/// Unenrolled ids return `None`. The output ceiling stays
/// [`claude_max_output_tokens`].
#[must_use]
pub fn claude_oauth_request_max_tokens(model: &str) -> Option<u32> {
    claude_enrollment(model)
        .filter(|row| row.oauth)
        .map(|row| row.request_max_tokens)
}

/// Observed Claude Code second system block (gateway identity).
pub const CLAUDE_OAUTH_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Plak User-Agent used by OAuth token exchange and non-inference traffic.
pub const CLAUDE_OAUTH_USER_AGENT: &str = concat!("plak/", env!("CARGO_PKG_VERSION"));
/// Claude Code 2.1.295 Messages `User-Agent` (edge costume, not token exchange).
pub const CLAUDE_OAUTH_MESSAGES_USER_AGENT: &str = "claude-cli/2.1.295 (external, cli)";
/// Stainless `x-stainless-lang` observed on Claude Code 2.1.270 Messages.
pub const CLAUDE_OAUTH_STAINLESS_LANG: &str = "js";
/// Stainless package version observed on Claude Code 2.1.295 Messages.
pub const CLAUDE_OAUTH_STAINLESS_PACKAGE_VERSION: &str = "0.128.0";
/// Stainless runtime observed on Claude Code 2.1.270 Messages.
pub const CLAUDE_OAUTH_STAINLESS_RUNTIME: &str = "node";
/// Stainless runtime version observed on Claude Code 2.1.295 Messages.
pub const CLAUDE_OAUTH_STAINLESS_RUNTIME_VERSION: &str = "v26.3.0";
/// Stainless retry count observed on a first Claude Code Messages attempt.
pub const CLAUDE_OAUTH_STAINLESS_RETRY_COUNT: &str = "0";
/// Stainless timeout seconds observed on Claude Code 2.1.295 Messages.
pub const CLAUDE_OAUTH_STAINLESS_TIMEOUT: &str = "600";

/// Stainless arch label matching the official CLI (`x64` / `arm64`).
#[must_use]
pub fn claude_oauth_stainless_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Stainless OS label matching the official CLI (`Linux` / `MacOS` / `Windows`).
#[must_use]
pub fn claude_oauth_stainless_os() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "MacOS",
        "windows" => "Windows",
        other => other,
    }
}
/// Default Claude OAuth coding model for the first enrolled profile.
pub const CLAUDE_OAUTH_DEFAULT_MODEL_ID: &str = "claude-sonnet-5";

/// Reviewed maximum output for sync Messages on an enrolled coding model.
///
/// Unknown ids fail closed to 64,000.
#[must_use]
pub fn claude_max_output_tokens(model: &str) -> u32 {
    claude_enrollment(model).map_or(CLAUDE_OTHER_MAX_OUTPUT_TOKENS, |row| row.max_output_tokens)
}

const CLAUDE_OTHER_MAX_OUTPUT_TOKENS: u32 = 64_000;

/// Builds the authorize URL for a PKCE Claude OAuth login.
///
/// Query order matches the Claude Code contract: `code=true`, client, response
/// type, redirect, scope, S256 challenge, `state` (the PKCE verifier).
#[must_use]
pub fn claude_authorize_url(code_challenge: &str, state: &str) -> String {
    format!(
        "{origin}{path}?code=true&client_id={client}&response_type=code&redirect_uri={redirect}&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
        origin = CLAUDE_OAUTH_AUTHORIZE_ORIGIN,
        path = CLAUDE_OAUTH_AUTHORIZE_PATH,
        client = urlencoding_unpadded(CLAUDE_OAUTH_CLIENT_ID),
        redirect = urlencoding_unpadded(CLAUDE_OAUTH_REDIRECT_URI),
        scope = urlencoding_unpadded(CLAUDE_OAUTH_AUTHORIZE_SCOPES),
        challenge = urlencoding_unpadded(code_challenge),
        state = urlencoding_unpadded(state),
    )
}

/// Extracts an authorization code from Claude Code paste input.
///
/// Accepts the HTTPS callback URL (current or legacy console), a `code=` query
/// string, OpenCode `code#state`, or a bare code. A present `state` must equal
/// `expected_state` (the PKCE verifier). A missing `state` is bound to that
/// verifier at token exchange.
#[must_use]
pub fn parse_claude_oauth_authorization_input(input: &str, expected_state: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || expected_state.is_empty() {
        return None;
    }
    let (code, state) = if let Some((code, state)) = split_code_hash_state(trimmed) {
        (code, Some(state))
    } else if let Some(query) = query_from_claude_paste(trimmed) {
        parse_code_state_query(query)?
    } else if is_bare_claude_code(trimmed) {
        (trimmed.to_owned(), None)
    } else {
        return None;
    };
    if code.is_empty() || code.len() > 16 * 1024 {
        return None;
    }
    if let Some(state) = state
        && state != expected_state
    {
        return None;
    }
    Some(code)
}

fn split_code_hash_state(input: &str) -> Option<(String, String)> {
    if input.contains("code=") || input.contains("://") {
        return None;
    }
    let (code, state) = input.split_once('#')?;
    if code.is_empty() || state.is_empty() || !is_bare_claude_code(code) || state.len() > 128 {
        return None;
    }
    Some((code.to_owned(), state.to_owned()))
}

fn query_from_claude_paste(input: &str) -> Option<&str> {
    for prefix in [
        "https://platform.claude.com/oauth/code/callback?",
        "https://console.anthropic.com/oauth/code/callback?",
    ] {
        if let Some(query) = input.strip_prefix(prefix) {
            return Some(query);
        }
    }
    if input.contains("://") {
        let query = input.split_once('?')?.1;
        if query.is_empty() || query.contains('#') {
            return None;
        }
        if input.starts_with(CLAUDE_OAUTH_REDIRECT_URI)
            || input.starts_with(CLAUDE_OAUTH_LEGACY_REDIRECT_URI)
        {
            return Some(query);
        }
        return None;
    }
    if input.contains("code=") && !input.contains(' ') {
        return Some(input);
    }
    None
}

fn parse_code_state_query(query: &str) -> Option<(String, Option<String>)> {
    if query.is_empty() || query.len() > 8 * 1024 || query.contains('#') {
        return None;
    }
    let mut code = None;
    let mut state = None;
    for field in query.split('&') {
        if field.is_empty() {
            return None;
        }
        let (key, value) = field.split_once('=')?;
        match key {
            "code" if code.is_none() && !value.is_empty() && value.len() <= 16 * 1024 => {
                code = Some(percent_decode_component(value)?);
            }
            "state" if state.is_none() && !value.is_empty() && value.len() <= 128 => {
                state = Some(percent_decode_component(value)?);
            }
            "scope" => {}
            _ => return None,
        }
    }
    Some((code?, state))
}

fn percent_decode_component(value: &str) -> Option<String> {
    let mut out = String::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hi = *bytes.get(index + 1)?;
                let lo = *bytes.get(index + 2)?;
                let decoded = (unhex(hi)? << 4) | unhex(lo)?;
                if !(0x20..=0x7e).contains(&decoded) {
                    return None;
                }
                out.push(char::from(decoded));
                index += 3;
            }
            b'+' => {
                out.push(' ');
                index += 1;
            }
            byte if (0x20..=0x7e).contains(&byte) => {
                out.push(char::from(byte));
                index += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

const fn unhex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_bare_claude_code(value: &str) -> bool {
    (16..=2_048).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/' | b'+' | b'=')
        })
}

fn urlencoding_unpadded(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*byte));
            }
            b' ' => out.push_str("%20"),
            _ => {
                out.push('%');
                out.push(char::from(hex(*byte >> 4)));
                out.push(char::from(hex(*byte & 0x0f)));
            }
        }
    }
    out
}

const fn hex(nibble: u8) -> u8 {
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'A' + (nibble - 10)
    }
}

/// JSON body for an authorization-code token exchange.
#[must_use]
pub fn claude_token_code_body(
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    state: &str,
) -> String {
    serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "client_id": CLAUDE_OAUTH_CLIENT_ID,
        "code_verifier": code_verifier,
        "state": state,
    })
    .to_string()
}

/// JSON body for a refresh-token exchange.
#[must_use]
pub fn claude_token_refresh_body(refresh_token: &str) -> String {
    serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLAUDE_OAUTH_CLIENT_ID,
        "scope": CLAUDE_OAUTH_REFRESH_SCOPES,
    })
    .to_string()
}

/// Parsed Claude OAuth token JSON.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeOAuthTokenGrant {
    account_uuid: Option<crate::ClaudeAccountUuid>,
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    scope: Option<String>,
}

impl ClaudeOAuthTokenGrant {
    /// Authenticated provider account UUID when returned by the grant.
    pub fn account_uuid(&self) -> Option<&crate::ClaudeAccountUuid> {
        self.account_uuid.as_ref()
    }
    /// Access token.
    #[must_use]
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
    /// Refresh token when present.
    #[must_use]
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }
    /// Lifetime in seconds.
    #[must_use]
    pub const fn expires_in(&self) -> i64 {
        self.expires_in
    }
    /// Space-delimited scope string.
    #[must_use]
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }
}

#[derive(Deserialize)]
struct TokenGrantAccount {
    #[serde(default)]
    uuid: Option<crate::ClaudeAccountUuid>,
}

#[derive(Deserialize)]
struct TokenGrantWire {
    #[serde(default)]
    account: Option<TokenGrantAccount>,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

/// Parses a Claude OAuth token JSON object.
///
/// # Errors
///
/// Rejects missing `access_token` or a non-positive `expires_in`.
pub fn parse_claude_oauth_token_grant(
    bytes: &[u8],
) -> Result<ClaudeOAuthTokenGrant, AnthropicError> {
    let wire: TokenGrantWire = serde_json::from_slice(bytes).map_err(|_| {
        AnthropicError::new(
            AnthropicErrorKind::InvalidRequest,
            AnthropicErrorPhase::Prepare,
            "Claude OAuth token JSON could not be parsed",
        )
    })?;
    if wire.access_token.is_empty() {
        return Err(AnthropicError::new(
            AnthropicErrorKind::InvalidRequest,
            AnthropicErrorPhase::Prepare,
            "Claude OAuth token JSON omitted access_token",
        ));
    }
    let expires_in = wire.expires_in.unwrap_or(0);
    if expires_in <= 0 {
        return Err(AnthropicError::new(
            AnthropicErrorKind::InvalidRequest,
            AnthropicErrorPhase::Prepare,
            "Claude OAuth token JSON omitted a positive expires_in",
        ));
    }
    Ok(ClaudeOAuthTokenGrant {
        account_uuid: wire.account.and_then(|account| account.uuid),
        access_token: wire.access_token,
        refresh_token: wire.refresh_token.filter(|value| !value.is_empty()),
        expires_in,
        scope: wire.scope.filter(|value| !value.is_empty()),
    })
}

/// Reports whether a scope string includes an admitted inference scope.
#[must_use]
pub fn claude_oauth_scope_admits_inference(scope: &str) -> bool {
    scope
        .split_ascii_whitespace()
        .any(|item| item == CLAUDE_OAUTH_INFERENCE_SCOPE)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests fail fast on invalid pin fixtures"
)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_uses_cai_not_platform_authorize() {
        let url = claude_authorize_url("challenge", "verifier-state");
        assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("code=true"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(!url.contains("platform.claude.com/oauth/authorize"));
    }

    #[test]
    fn token_grant_requires_access_and_expiry() {
        let grant = parse_claude_oauth_token_grant(
            br#"{"access_token":"at","refresh_token":"rt","expires_in":3600,"scope":"user:inference"}"#,
        )
        .expect("grant");
        assert_eq!(grant.access_token(), "at");
        assert_eq!(grant.refresh_token(), Some("rt"));
        assert_eq!(grant.expires_in(), 3600);
        assert!(claude_oauth_scope_admits_inference(grant.scope().unwrap()));
        assert!(parse_claude_oauth_token_grant(br#"{"access_token":"at"}"#).is_err());
    }

    #[test]
    fn compatibility_envelope_is_pinned_to_claude_code_messages_costume() {
        // Re-pinning inference must not strand an already selected login or
        // demand a new compatibility acknowledgement for unchanged auth.
        assert_eq!(
            CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            "anthropic.claude.oauth.compat.2026-09-22.r2"
        );
        assert_eq!(CLAUDE_OAUTH_SOURCE_REVISION, "claude-oauth.2026-09-22.v3");
        assert!(!CLAUDE_OAUTH_BASELINE_BETAS.contains("context-1m"));
        assert!(!CLAUDE_OAUTH_BASELINE_BETAS.contains("per-turn-control"));
        assert_eq!(CLAUDE_OAUTH_STAINLESS_PACKAGE_VERSION, "0.128.0");
        assert!(CLAUDE_OAUTH_USER_AGENT.starts_with("plak/"));
        assert_eq!(
            CLAUDE_OAUTH_MESSAGES_USER_AGENT,
            "claude-cli/2.1.295 (external, cli)"
        );
        assert_eq!(
            crate::CLAUDE_OAUTH_BILLING_HEADER,
            "x-anthropic-billing-header: cc_version=2.1.295.8a8; cc_entrypoint=cli; cch=00000;"
        );
        assert_eq!(
            CLAUDE_OAUTH_IDENTITY,
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert!(!claude_oauth_stainless_arch().is_empty());
        assert!(!claude_oauth_stainless_os().is_empty());
    }

    #[test]
    fn enrolled_effort_options_are_exact_per_model_and_unknowns_fail_closed() {
        for model in [
            "claude-sonnet-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-opus-5-5",
        ] {
            assert_eq!(
                claude_oauth_reasoning_effort_options(model),
                &[
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::XHigh,
                    ReasoningEffort::Max,
                ],
                "{model} must retain its exact effort set"
            );
        }
        assert_eq!(
            claude_oauth_reasoning_effort_options("claude-sonnet-5-5"),
            CLAUDE_OAUTH_EXTENDED_REASONING_EFFORTS
        );
        for dropped in [
            // Adjacent Platform or client documentation alone does not qualify
            // a subscription model.
            "claude-fable-5",
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-haiku-4-5",
            "claude-fable-5-neighbor",
        ] {
            assert!(
                claude_oauth_reasoning_effort_options(dropped).is_empty(),
                "{dropped} must fail closed"
            );
        }
    }

    #[test]
    fn enrolled_context_and_output_facts_come_from_one_table_and_unknowns_fail_closed() {
        assert_eq!(
            claude_oauth_enrolled_models().collect::<Vec<_>>(),
            [
                "claude-sonnet-5",
                "claude-sonnet-5-5",
                "claude-haiku-5-5",
                "claude-fable-5-1",
                "claude-opus-5",
                "claude-opus-5-5",
            ]
        );
        for model in [
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-opus-5-5",
        ] {
            let row = claude_enrollment(model).expect("enrolled");
            assert_eq!(row.context_window, 1_000_000);
            assert!(claude_oauth_enrolled_model(model));
            assert_eq!(claude_max_output_tokens(model), 128_000);
            assert_eq!(
                claude_oauth_beta_header(model),
                row.oauth_betas,
                "{model} beta header must come from its enrollment row"
            );
        }
        let expected = [
            ("claude-sonnet-5", 64_000, false, false),
            ("claude-sonnet-5-5", 128_000, true, true),
            ("claude-haiku-5-5", 128_000, true, true),
            ("claude-fable-5-1", 64_000, true, true),
            ("claude-opus-5", 64_000, false, true),
            ("claude-opus-5-5", 128_000, true, true),
        ];
        for (model, request_max, per_turn, mid_tools) in expected {
            let row = claude_enrollment(model).expect("enrolled");
            assert_eq!(row.request_max_tokens, request_max, "{model}");
            assert_eq!(
                claude_oauth_request_max_tokens(model),
                Some(request_max),
                "{model}"
            );
            let header = row.oauth_betas;
            assert_eq!(
                header.contains("per-turn-control-2026-07-01"),
                per_turn,
                "{model}"
            );
            assert_eq!(
                header.contains("mid-conversation-tool-changes-2026-07-01"),
                mid_tools,
                "{model}"
            );
            assert!(!header.contains("context-1m"), "{model}");
            let tokens: Vec<&str> = header.split(',').collect();
            let code_at = tokens
                .iter()
                .position(|token| *token == "claude-code-20250219")
                .expect("claude-code beta");
            if model.contains("haiku") {
                assert_eq!(code_at, 6, "{model} puts claude-code seventh");
            } else {
                assert_eq!(code_at, 0, "{model} puts claude-code first");
            }
        }
        assert_eq!(
            claude_enrollment("claude-fable-5-1")
                .expect("fable")
                .oauth_betas,
            claude_enrollment("claude-sonnet-5-5")
                .expect("sonnet 5.5")
                .oauth_betas,
            "fable has both registry flags and is not Haiku"
        );
        assert_eq!(
            claude_oauth_beta_header("claude-sonnet-5"),
            CLAUDE_OAUTH_BASELINE_BETAS
        );
        assert!(claude_oauth_request_max_tokens("claude-haiku-4-5").is_none());
        let haiku = claude_enrollment("claude-haiku-5-5").expect("haiku 5.5");
        assert_eq!(haiku.family, "claude-5-haiku");
        assert!(haiku.oauth);
        for model in ["claude-haiku-4-5", "claude-future-neighbor"] {
            assert!(claude_enrollment(model).is_none());
            assert_eq!(claude_max_output_tokens(model), 64_000);
            assert!(claude_oauth_reasoning_effort_options(model).is_empty());
        }
    }

    #[test]
    fn paste_input_accepts_callback_url_query_hash_and_bare_code() {
        let verifier = "a".repeat(43);
        let code = "b".repeat(32);
        let url = format!("{CLAUDE_OAUTH_REDIRECT_URI}?code={code}&state={verifier}");
        assert_eq!(
            parse_claude_oauth_authorization_input(&url, &verifier).as_deref(),
            Some(code.as_str())
        );
        assert_eq!(
            parse_claude_oauth_authorization_input(
                &format!("{CLAUDE_OAUTH_REDIRECT_URI}?code={code}"),
                &verifier
            )
            .as_deref(),
            Some(code.as_str())
        );
        assert_eq!(
            parse_claude_oauth_authorization_input(&format!("code={code}"), &verifier).as_deref(),
            Some(code.as_str())
        );
        assert_eq!(
            parse_claude_oauth_authorization_input(&format!("{code}#{verifier}"), &verifier)
                .as_deref(),
            Some(code.as_str())
        );
        assert_eq!(
            parse_claude_oauth_authorization_input(&code, &verifier).as_deref(),
            Some(code.as_str())
        );
        assert!(
            parse_claude_oauth_authorization_input(
                &format!(
                    "{CLAUDE_OAUTH_REDIRECT_URI}?code={code}&state=other-state-value-xxxxxxxx"
                ),
                &verifier
            )
            .is_none()
        );
        assert!(parse_claude_oauth_authorization_input("nope", &verifier).is_none());
        assert!(
            parse_claude_oauth_authorization_input(
                "https://evil.example/oauth/code/callback?code=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &verifier
            )
            .is_none()
        );
    }
}
