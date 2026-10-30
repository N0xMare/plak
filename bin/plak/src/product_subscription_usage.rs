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
mod tests;
