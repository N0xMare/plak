//! Local warnings for every compiled provider review, independent of credentials.

use std::io::{self, Write};

const DAY: u64 = 86_400;
const WARNING_WINDOW: u64 = 14 * DAY;

/// How a compiled review date binds its route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewClass {
    /// A subscription compatibility profile: the route refuses at the deadline.
    HardExpiry { route: &'static str },
    /// Documented public API facts: the deadline only asks for a re-review.
    ReviewDue,
}

fn compiled_reviews() -> Vec<(&'static str, ReviewClass, u64)> {
    [
        (
            "codex",
            "Codex OAuth",
            ReviewClass::HardExpiry { route: "Codex" },
        ),
        ("openrouter", "OpenRouter Chat", ReviewClass::ReviewDue),
        ("xai-api", "xAI Platform API", ReviewClass::ReviewDue),
        ("openai", "OpenAI Platform API", ReviewClass::ReviewDue),
        (
            "grok",
            "Grok OAuth",
            ReviewClass::HardExpiry { route: "Grok" },
        ),
        (
            "claude",
            "Claude OAuth",
            ReviewClass::HardExpiry { route: "Claude" },
        ),
        ("anthropic", "Anthropic API", ReviewClass::ReviewDue),
    ]
    .into_iter()
    .filter_map(|(account, label, class)| {
        plakous_hosted::capabilities::for_account(account)?
            .review_deadline()
            .map(|deadline| (label, class, deadline))
    })
    .collect()
}

/// Writes one line per review within 14 days of its deadline or past it and
/// returns how many it wrote.
pub(crate) fn write_warnings(stdout: &mut dyn Write, now: u64) -> io::Result<usize> {
    let mut count = 0;
    for (surface, class, deadline) in compiled_reviews() {
        if deadline > now.saturating_add(WARNING_WINDOW) {
            continue;
        }
        let date = crate::presentation::format_unix_seconds(deadline);
        let passed = deadline <= now;
        match class {
            ReviewClass::HardExpiry { route } if passed => writeln!(
                stdout,
                "Warning: {surface} review expired {date}; this build refuses {route}. Install a newly reviewed build."
            )?,
            ReviewClass::HardExpiry { route } => writeln!(
                stdout,
                "Warning: {surface} review expires {date}; this build refuses {route} after it. Install a newly reviewed build."
            )?,
            ReviewClass::ReviewDue => writeln!(
                stdout,
                "Note: {surface} facts {} due for review on {date}; the route keeps working. A newer build carries re-reviewed facts.",
                if passed { "were" } else { "are" }
            )?,
        }
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warnings_at(now: u64) -> io::Result<(usize, String)> {
        let mut output = Vec::new();
        let count = write_warnings(&mut output, now)?;
        Ok((count, String::from_utf8_lossy(&output).into_owned()))
    }

    #[test]
    fn every_surface_warns_from_fourteen_days_before_its_deadline() -> io::Result<()> {
        for (surface, _, deadline) in compiled_reviews() {
            for now in [deadline - 14 * DAY, deadline, deadline + 1] {
                let (_, output) = warnings_at(now)?;
                assert!(output.contains(surface), "{surface} at {now}");
                assert!(output.contains(&crate::presentation::format_unix_seconds(deadline)));
            }
            let (_, output) = warnings_at(deadline - 14 * DAY - 1)?;
            assert!(!output.contains(surface), "{surface}");
        }
        Ok(())
    }

    #[test]
    fn subscription_routes_warn_of_refusal_and_api_routes_of_a_due_review() -> io::Result<()> {
        for (surface, class, deadline) in compiled_reviews() {
            let line = |now| -> io::Result<String> {
                let (_, output) = warnings_at(now)?;
                Ok(output
                    .lines()
                    .find(|line| line.contains(surface))
                    .unwrap_or_default()
                    .to_owned())
            };
            let date = crate::presentation::format_unix_seconds(deadline);
            let (before, after) = (line(deadline - 1)?, line(deadline)?);
            match class {
                ReviewClass::HardExpiry { route } => {
                    assert_eq!(
                        before,
                        format!(
                            "Warning: {surface} review expires {date}; this build refuses {route} after it. Install a newly reviewed build."
                        )
                    );
                    assert_eq!(
                        after,
                        format!(
                            "Warning: {surface} review expired {date}; this build refuses {route}. Install a newly reviewed build."
                        )
                    );
                }
                ReviewClass::ReviewDue => {
                    assert_eq!(
                        before,
                        format!(
                            "Note: {surface} facts are due for review on {date}; the route keeps working. A newer build carries re-reviewed facts."
                        )
                    );
                    assert_eq!(
                        after,
                        format!(
                            "Note: {surface} facts were due for review on {date}; the route keeps working. A newer build carries re-reviewed facts."
                        )
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn the_seven_surfaces_split_into_three_hard_expiries_and_four_review_dates() {
        let reviews = compiled_reviews();
        let hard = reviews
            .iter()
            .filter(|(_, class, _)| matches!(class, ReviewClass::HardExpiry { .. }))
            .count();
        assert_eq!((hard, reviews.len() - hard), (3, 4));
    }

    #[test]
    fn warnings_are_credential_independent_and_counted() -> io::Result<()> {
        let after_all_deadlines = compiled_reviews()
            .into_iter()
            .map(|(_, _, deadline)| deadline)
            .max()
            .unwrap_or(0);
        assert_eq!(warnings_at(after_all_deadlines)?.0, 7);
        Ok(())
    }
}
