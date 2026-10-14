//! Context usage percentage test helper.

/// Half-up percent of window used.
#[must_use]
pub fn context_percent(tokens: u64, window: u64) -> u64 {
    if window == 0 {
        return 0;
    }
    (tokens.saturating_mul(100).saturating_add(window / 2)) / window
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_half_up() {
        assert_eq!(context_percent(0, 128_000), 0);
        assert_eq!(context_percent(64_000, 128_000), 50);
        assert_eq!(context_percent(1_280, 128_000), 1);
    }
}
