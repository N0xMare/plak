//! Refresh failure classification shared by status, doctor and Usage.

use super::current_credential_generation;

/// A failure row belongs to the record its claim left behind: quarantine keeps
/// the claim generation and a restore advances it once. A record newer than
/// that (a later login, logout, or refresh) makes the row history, not state.
pub(super) fn refresh_row_is_current(row_generation: u64, record_generation: Option<u64>) -> bool {
    record_generation.is_none_or(|current| current <= row_generation.saturating_add(1))
}

/// The newest terminal refresh outcome is display evidence only, never admission
/// or recovery authority. A later Began row does not erase the last failure.
pub(crate) fn last_failed_refresh(
    paths: &crate::product_paths::PlakPaths,
    account: &str,
    zone: &jiff::tz::TimeZone,
) -> Option<String> {
    let provider = match account {
        "grok" | "xai" | "supergrok" => "xai",
        "codex" => "codex",
        "claude" => "claude",
        _ => return None,
    };
    let row =
        crate::telemetry_store::TelemetryStore::latest_refresh_outcome(paths, provider).ok()??;
    if !refresh_row_is_current(row.generation, current_credential_generation(provider)) {
        return None;
    }
    let reason = row.failure_reason.as_deref();
    let at = jiff::Timestamp::from_nanosecond(i128::from(row.at_ns)).ok()?;
    let local = at.to_zoned(zone.clone());
    let clock = local.strftime("%H:%M");
    let class = row.failure_class.as_deref();
    let cause = match (class, row.transport_phase.as_deref()) {
        (Some("timeout"), Some("resolution")) => {
            "network unreachable (DNS lookup timed out)".to_owned()
        }
        (Some("transport"), Some("resolution")) => {
            "network unreachable (DNS lookup failed)".to_owned()
        }
        (Some("transport" | "timeout"), Some("connect")) => {
            "network unreachable (could not connect to the provider)".to_owned()
        }
        (Some("transport" | "timeout"), Some("tls")) => {
            "TLS handshake failed (a captive portal or intercepting proxy can cause this)"
                .to_owned()
        }
        (Some("transport" | "timeout"), Some("send")) => {
            "connection dropped while sending or waiting for a response".to_owned()
        }
        (Some("transport" | "timeout"), Some("response")) => {
            "connection interrupted while receiving the response".to_owned()
        }
        (Some("transport"), _) => "network transport failed (phase not recorded)".to_owned(),
        (Some("timeout"), _) => "refresh timed out (phase not recorded)".to_owned(),
        (Some("http_4xx" | "http_5xx" | "http_other"), _) => row.http_status.map_or_else(
            || "provider rejected the refresh".to_owned(),
            |status| format!("provider returned HTTP {status}"),
        ),
        (Some("validation"), _) if reason == Some("retired_format") => {
            "the stored credential was written by an older plak build (no request was sent)"
                .to_owned()
        }
        (Some("validation"), _) if reason == Some("stored_credential") => {
            "the stored credential could not be read (no request was sent)".to_owned()
        }
        (Some("validation"), _) => reason.map_or_else(
            || "refresh response could not be validated".to_owned(),
            |reason| format!("refresh response could not be validated ({reason})"),
        ),
        (Some("store"), _) => "credential storage failure".to_owned(),
        (Some("cancelled"), _) => "refresh cancelled".to_owned(),
        (Some("not_ready"), _) => "refresh dispatch was not ready".to_owned(),
        _ => "cause not recorded".to_owned(),
    };
    let state = match row.phase.as_str() {
        "restored" => "credential unchanged",
        "quarantined" => "credential quarantined",
        _ => "refresh outcome unknown",
    };
    let remedy = if row.phase == "restored" && matches!(class, Some("transport" | "timeout")) {
        "retry when online".to_owned()
    } else if matches!(reason, Some("retired_format" | "stored_credential")) {
        format!("run plak login {account}")
    } else if provider == "xai" {
        "run plak refresh grok; run plak login grok if that fails".to_owned()
    } else if row.phase == "restored" {
        format!("run plak refresh {account}; run plak login {account} if that fails")
    } else {
        format!("run plak login {account}")
    };
    Some(format!(
        "refresh failed at {clock} (generation {}): {cause}; OAuth {}: {}; {state}; {remedy}",
        row.generation,
        row.oauth_error_code.as_deref().unwrap_or("not recorded"),
        row.oauth_error_description
            .as_deref()
            .unwrap_or("not recorded"),
    ))
}
