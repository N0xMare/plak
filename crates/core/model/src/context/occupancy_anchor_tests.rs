use super::{ContextAnchor, context_occupancy_tokens};

#[test]
fn anchored_occupancy_is_provider_input_plus_the_suffix_estimate() {
    // The provider counted 298,468 tokens for a request estimated at
    // 850,715; 4,000 bytes appended since then add 1,000 tokens.
    let anchor = ContextAnchor::new(298_468, 850_715);
    assert_eq!(
        context_occupancy_tokens(850_715 + 1_000, anchor),
        298_468 + 1_000
    );
    assert_eq!(context_occupancy_tokens(850_715, anchor), 298_468);
}

/// Continuation frames decode their anchor through serde, so the
/// positive-count invariant holds there too.
#[test]
fn a_decoded_context_anchor_rejects_a_zero_provider_count() -> serde_json::Result<()> {
    let decoded: ContextAnchor = serde_json::from_str(
        r#"{"provider_input_tokens":298468,"request_estimate_tokens":850715}"#,
    )?;
    assert_eq!(Some(decoded), ContextAnchor::new(298_468, 850_715));
    assert!(
        serde_json::from_str::<ContextAnchor>(
            r#"{"provider_input_tokens":0,"request_estimate_tokens":850715}"#
        )
        .is_err(),
        "a zero provider count is not an anchor, on any decode path"
    );
    Ok(())
}

#[test]
fn without_a_provider_report_occupancy_is_the_whole_estimate() {
    assert_eq!(context_occupancy_tokens(850_715, None), 850_715);
    assert_eq!(ContextAnchor::new(0, 850_715), None);
}

#[test]
fn an_estimate_below_the_anchored_request_adds_no_suffix() {
    let anchor = ContextAnchor::new(10_000, 30_000);
    assert_eq!(context_occupancy_tokens(20_000, anchor), 10_000);
}
