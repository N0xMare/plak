//! What the repository gate's build reports that a production build drops
//! silently (`test-hooks`, never in `default`).

pub mod commit_classes;

/// A credential-wait clear the event hub refused: the host keeps showing the
/// wait, so a gate run names the refusal on stderr.
pub(crate) fn credential_wait_clear_refused(error: &dyn std::fmt::Debug) {
    eprintln!("plakous-agent test-hooks: a credential-wait clear was not published: {error:?}");
}

/// The mid-turn prune minimum: `PLAK_TEST_WORKING_PRUNE_MINIMUM` when set to a
/// positive count, so a gate sweep prunes between every pair of rounds.
pub(crate) fn working_prune_minimum(production: u64) -> u64 {
    std::env::var("PLAK_TEST_WORKING_PRUNE_MINIMUM")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|minimum| *minimum > 0)
        .unwrap_or(production)
}

/// The Working-head recency margin: `PLAK_TEST_WORKING_PRUNE_MARGIN` when it
/// parses as a count, including 0. Absent or unparsable, the production margin.
/// A crash sweep sets it beside the minimum so a short turn still prunes.
pub(crate) fn working_prune_margin(production: u64) -> u64 {
    std::env::var("PLAK_TEST_WORKING_PRUNE_MARGIN")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(production)
}

/// A lower operation-only high-water for short deterministic retirement pilots.
pub(crate) fn ledger_retirement_high_water(production: usize) -> usize {
    std::env::var("PLAK_TEST_LEDGER_RETIREMENT_HIGH_WATER")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| (8..=production).contains(value))
        .unwrap_or(production)
}

/// Lower admission headroom without consuming the production settlement reserve.
pub(crate) fn ledger_headroom_limit(production: usize) -> usize {
    std::env::var("PLAK_TEST_LEDGER_HEADROOM_LIMIT")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| (1..=production).contains(value))
        .unwrap_or(production)
}

// Mid-turn retention calls before this turn has a provider round. The round
// loop must not call retention on its first iteration. `#[tokio::test]` polls
// on the test thread, so a thread-local stays with that test.
#[cfg(test)]
thread_local! {
    static WORKING_PRUNE_BEFORE_PROVIDER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn note_working_prune_before_provider() {
    WORKING_PRUNE_BEFORE_PROVIDER.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(test)]
pub(crate) fn working_prune_before_provider() -> usize {
    WORKING_PRUNE_BEFORE_PROVIDER.with(std::cell::Cell::get)
}
