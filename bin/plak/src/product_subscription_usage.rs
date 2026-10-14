//! CLI rendering over the one host-owned Usage projection.
//!
//! There is no second projection and no second renderer here: every `plak
//! usage` surface goes through [`write_limits`], which is
//! `plakous_hosted::subscription_usage::render_limits_text`, so the CLI and the TUI
//! Usage surface can never disagree.

use std::io::{self, Write};

use crate::product_paths::PlakPaths;
use plakous_host_protocol::views::presentation::{UsageLimitsView, VerticalUsageState};
use plakous_hosted::subscription_usage as host_usage;

/// Writes the shared Limits block for one complete projection.
pub(crate) fn write_limits(
    view: &UsageLimitsView,
    paths: Option<&PlakPaths>,
    zone: &jiff::tz::TimeZone,
    output: &mut dyn Write,
) -> io::Result<()> {
    let mut display = view.clone();
    let notes = annotate_limits(&mut display, paths, zone);
    write!(output, "{}", host_usage::render_limits_text(&display))?;
    for note in notes {
        writeln!(output, "{note}")?;
    }
    Ok(())
}

/// Decorates only the product's display copy. The host-owned usage facts and
/// credential state are never changed or fed back into admission/recovery.
pub(crate) fn annotate_limits(
    display: &mut UsageLimitsView,
    paths: Option<&PlakPaths>,
    zone: &jiff::tz::TimeZone,
) -> Vec<String> {
    let Some(paths) = paths else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    for vertical in &mut display.verticals {
        if matches!(vertical.state, VerticalUsageState::NotConnected) {
            continue;
        }
        let Some(failure) =
            crate::product_doctor::last_failed_refresh(paths, vertical.vertical.account(), zone)
        else {
            continue;
        };
        match &mut vertical.state {
            VerticalUsageState::Unavailable { reason }
                if reason.starts_with("credential needs a token refresh;")
                    || reason
                        .starts_with("credential is quarantined after an ambiguous refresh;") =>
            {
                *reason = failure;
            }
            _ => notes.push(format!("{}: {failure}", vertical.vertical.provider_label())),
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use host_usage::VerticalObservation;
    use plakous::model::{
        SubscriptionUsagePercent, SubscriptionUsagePeriod, SubscriptionUsageRead,
        SubscriptionUsageSnapshot, SubscriptionUsageUnavailable, SubscriptionUsageWindow,
        Timestamp,
    };
    use plakous_host_protocol::views::presentation::SessionLedgerView;

    #[test]
    fn usage_last_failed_refresh_fixture_replaces_only_refresh_refusals()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout()?;
        drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
        let db = rusqlite::Connection::open(paths.telemetry_database())?;
        db.execute_batch(
            "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, transport_phase, generation) VALUES
            (1720000100000000000, 'claude', 'restored', 'timeout', 'resolution', 2);",
        )?;
        let read = SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(1_720_000_200, 0)?,
            reason: SubscriptionUsageUnavailable::CredentialState(
                plakous::model::SubscriptionUsageCredentialState::RefreshRequired,
            ),
        };
        let view = host_usage::project_limits(
            vec![(
                host_usage::claude_vertical(),
                VerticalObservation::Native(read),
            )],
            SessionLedgerView::default(),
        );
        let zone = jiff::tz::TimeZone::fixed(jiff::tz::offset(-4));
        let mut rendered = Vec::new();
        write_limits(&view, Some(&paths), &zone, &mut rendered)?;
        let text = String::from_utf8(rendered)?;
        assert!(text.contains("refresh failed at 05:48: network unreachable (DNS lookup timed out); credential unchanged; retry when online"), "{text}");
        assert!(!text.contains("credential needs a token refresh"), "{text}");
        // The canonical host snapshot is untouched by display decoration.
        assert!(host_usage::render_limits_text(&view).contains("credential needs a token refresh"));

        let mut display = view.clone();
        let claude = display
            .verticals
            .iter_mut()
            .find(|v| v.vertical.account() == "claude")
            .ok_or("Claude missing")?;
        claude.state = VerticalUsageState::Ready { meters: Vec::new() };
        let notes = annotate_limits(&mut display, Some(&paths), &zone);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("refresh failed at 05:48"));
        assert!(matches!(
            display
                .verticals
                .iter()
                .find(|v| v.vertical.account() == "claude")
                .ok_or("Claude missing")?
                .state,
            VerticalUsageState::Ready { .. }
        ));
        Ok(())
    }

    #[test]
    fn combined_projection_preserves_provider_specific_unavailability()
    -> Result<(), Box<dyn std::error::Error>> {
        let observed = Timestamp::new(1_788_000_000, 0)?;
        let codex = SubscriptionUsageRead::Snapshot(SubscriptionUsageSnapshot::new(
            observed,
            Some("ChatGPT Pro".to_owned()),
            vec![plakous::model::SubscriptionUsageMeter::new(
                "requests".into(),
                None,
                vec![SubscriptionUsageWindow::new(
                    Some("primary".into()),
                    SubscriptionUsagePercent::new(12.5)?,
                    Some(SubscriptionUsagePeriod::Rolling {
                        duration_seconds: 18_000,
                    }),
                    Some(Timestamp::new(1_788_018_000, 0)?),
                )?],
            )?],
        )?);
        let claude = SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(1_788_003_600, 0)?,
            reason: SubscriptionUsageUnavailable::Unqualified,
        };
        let view = host_usage::project_limits(
            vec![
                (
                    host_usage::codex_vertical(),
                    VerticalObservation::Native(codex),
                ),
                (
                    host_usage::claude_vertical(),
                    VerticalObservation::Native(claude),
                ),
            ],
            SessionLedgerView::default(),
        );
        let mut rendered = Vec::new();
        write_limits(&view, None, &jiff::tz::TimeZone::UTC, &mut rendered)?;
        let text = String::from_utf8(rendered)?;
        assert!(text.contains("ChatGPT Pro"), "{text}");
        // 12.5% used is 87% left, floored.
        assert!(text.contains("87% left"), "{text}");
        // Every vertical is listed, and an unavailable one keeps its own
        // provider-specific reason rather than borrowing another's.
        assert!(text.contains("Claude"), "{text}");
        assert!(text.contains("SuperGrok"), "{text}");
        assert!(text.contains("OpenRouter"), "{text}");
        Ok(())
    }
}
