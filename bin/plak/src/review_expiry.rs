//! Local warnings for every compiled provider review, independent of credentials.

use std::io::{self, Write};

use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

const DAY: u64 = 86_400;

fn compiled_reviews() -> [(&'static str, u64); 7] {
    [
        (
            "Codex OAuth",
            NativeSubscriptions::notice(NativeProvider::Codex).expires_at(),
        ),
        (
            "OpenRouter Chat",
            plakous::openrouter::OPENROUTER_CHAT_REVIEW_EXPIRES_AT,
        ),
        (
            "xAI Platform API",
            plakous::xai::XAI_PLATFORM_REVIEW_EXPIRES_AT_UNIX_SECONDS,
        ),
        (
            "OpenAI Platform API",
            plakous::openai::OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS,
        ),
        (
            "Grok OAuth",
            NativeSubscriptions::notice(NativeProvider::Xai).expires_at(),
        ),
        (
            "Claude OAuth",
            NativeSubscriptions::notice(NativeProvider::Claude).expires_at(),
        ),
        (
            "Anthropic API",
            plakous::anthropic::ANTHROPIC_PLATFORM_ADMISSION_EXPIRES_AT_UNIX_SECONDS,
        ),
    ]
}

pub(crate) fn write_warnings(stdout: &mut dyn Write, now: u64) -> io::Result<usize> {
    let mut count = 0;
    for (surface, expiry) in compiled_reviews() {
        if expiry > now.saturating_add(14 * DAY) {
            continue;
        }
        let verb = if expiry <= now { "expired" } else { "expires" };
        writeln!(
            stdout,
            "Warning: {surface} review {verb} {}; install a newly reviewed build.",
            crate::presentation::format_unix_seconds(expiry),
        )?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_surface_warns_at_fourteen_days_and_after_expiry() -> io::Result<()> {
        for (surface, expiry) in compiled_reviews() {
            for now in [expiry - 14 * DAY, expiry, expiry + 1] {
                let mut output = Vec::new();
                write_warnings(&mut output, now)?;
                let output = String::from_utf8_lossy(&output);
                assert!(output.contains(surface));
                assert!(output.contains(&crate::presentation::format_unix_seconds(expiry)));
                assert!(output.contains("install a newly reviewed build"));
            }
            let mut output = Vec::new();
            write_warnings(&mut output, expiry - 14 * DAY - 1)?;
            assert!(!String::from_utf8_lossy(&output).contains(surface));
        }
        Ok(())
    }

    #[test]
    fn warnings_are_credential_independent_and_counted() -> io::Result<()> {
        let after_all_expiries = compiled_reviews()
            .map(|(_, expiry)| expiry)
            .into_iter()
            .max();
        let mut output = Vec::new();
        assert_eq!(
            write_warnings(&mut output, after_all_expiries.unwrap_or(0))?,
            7
        );
        Ok(())
    }
}
