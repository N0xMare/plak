use plakd_wire_types::console::ConsoleLimits;
use plakous_hosted::host_service::wire::HostReadLimits;
use std::time::Duration;

pub(super) const RESPONSE_BYTES: usize = 256 * 1024;
pub(super) const DEFINITION_BYTES: usize = 256 * 1024;
pub(super) const PAGE_ROWS: u64 = 100;
pub(super) const DEPENDENCY_BYTES: usize = 8 * 1024 * 1024;
pub(super) const SCAN_ENTRIES: usize = 512;
pub(super) const PROVIDER_READS: usize = 1;
pub(super) const PROVIDER_DEADLINE: Duration = Duration::from_secs(5);
pub(super) const CURSOR_TTL: Duration = Duration::from_secs(30);
pub(super) const SNAPSHOTS: usize = 32;
pub(super) const CURSORS_PER_PRINCIPAL: usize = 8;
pub(super) const PROVIDER_RESERVE: Duration = Duration::from_secs(1);

pub(super) fn admission() -> HostReadLimits {
    HostReadLimits {
        request_bytes: 272 * 1024,
        per_connection: 2,
        per_principal: 4,
        host: 32,
        deadline: Duration::from_secs(10),
        messages_per_second: 20,
        burst: 60,
    }
}

pub(super) fn welcome(limits: &HostReadLimits) -> ConsoleLimits {
    let freshness = plakous::subscriptions::USAGE_OBSERVATION_FRESHNESS;
    ConsoleLimits {
        confirmation_ttl_ms: 60_000,
        confirmations_per_principal: 4,
        credential_mutations_per_minute: 10,
        config_mutations_per_minute: 30,
        login_starts_per_minute: 3,
        pending_logins_per_principal: 2,
        pending_logins_host: 3,
        login_lifetime_ms: 900_000,
        login_poll_ms: 1_000,
        secret_input_bytes: crate::presentation::MAX_REMOTE_AUTHORIZATION_INPUT_BYTES as u64,
        credential_request_bytes: 64 * 1024,
        max_request_bytes: limits.request_bytes as u64,
        max_response_bytes: RESPONSE_BYTES as u64,
        max_definition_bytes: DEFINITION_BYTES as u64,
        page_rows: PAGE_ROWS,
        dependency_bytes: DEPENDENCY_BYTES as u64,
        dependency_definitions: SCAN_ENTRIES as u64,
        message_rate_per_second: limits.messages_per_second,
        message_burst: limits.burst,
        inflight_per_connection: limits.per_connection as u64,
        inflight_per_principal: limits.per_principal as u64,
        inflight_host: limits.host as u64,
        provider_reads_inflight_host: PROVIDER_READS as u64,
        provider_deadline_ms: milliseconds(PROVIDER_DEADLINE),
        request_deadline_ms: milliseconds(limits.deadline),
        usage_cache_ms: milliseconds(freshness),
        usage_reread_ms: milliseconds(freshness),
        cursor_ttl_ms: milliseconds(CURSOR_TTL),
    }
}
fn milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_limits_match_the_documented_enforced_defaults()
    -> Result<(), Box<dyn std::error::Error>> {
        let spec = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/spec/07-config-reference.md"),
        )?;
        let value = serde_json::to_value(welcome(&admission()))?;
        let limits = value.as_object().ok_or("limits object")?;
        assert_eq!(limits.len(), 28);
        for (key, value) in limits {
            let number = value.as_u64().ok_or("integer limit")?;
            assert!(
                spec.lines()
                    .any(|line| line.starts_with(&format!("| `{key}` | `{number}` |"))),
                "{key} default must match spec 07"
            );
        }
        Ok(())
    }
}
