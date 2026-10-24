use super::observation_budget_cap_bytes;

#[test]
fn hosted_observation_cap_tracks_the_effective_input_window_in_text_bytes() {
    for (tokens, bytes) in [
        (1, 32_768),
        (100_000, 32_768),
        (163_840, 52_428),
        (256_000, 81_920),
        (258_400, 82_688),
        (500_000, 160_000),
        (1_000_000, 262_144),
        (u64::MAX, 262_144),
    ] {
        assert_eq!(
            observation_budget_cap_bytes(tokens),
            Some(bytes),
            "{tokens}"
        );
    }
}
