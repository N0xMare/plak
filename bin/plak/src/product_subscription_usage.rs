//! CLI rendering over the one host-owned Usage projection.
//!
//! The host owns usage facts. Plak decorates a display copy with typed repair
//! guidance before passing it to the shared neutral Limits formatter.

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
    for vertical in &mut display.verticals {
        let account = vertical.vertical.account();
        match &mut vertical.state {
            VerticalUsageState::NotConnected => {
                vertical.state = VerticalUsageState::Unavailable {
                    reason: crate::usage_remedy::not_connected(account),
                }
            }
            VerticalUsageState::Unavailable { reason } => {
                *reason = crate::usage_remedy::unavailable_reason(vertical.repair, account, reason);
            }
            VerticalUsageState::Ready { .. } => {}
        }
    }
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
        match (&mut vertical.state, vertical.repair) {
            (
                VerticalUsageState::Unavailable { reason },
                Some(plakous_host_protocol::views::UsageRepair::Credential(
                    plakous_host_protocol::views::UsageCredentialState::RefreshRequired
                    | plakous_host_protocol::views::UsageCredentialState::Quarantined,
                )),
            ) => {
                *reason = failure;
                vertical.repair = None;
            }
            _ => notes.push(format!("{}: {failure}", vertical.vertical.provider_label())),
        }
    }
    notes
}

#[cfg(test)]
mod tests;
