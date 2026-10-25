use super::accepted_duplicate_consumed;
use crate::sessions;

#[allow(clippy::unwrap_used, reason = "test assertions")]
#[test]
fn accepted_duplicate_settles_from_the_fact_or_the_retained_feed_floor() {
    let mut views = sessions::projector::SessionViews::empty();
    for retained_from in [None, Some(0)] {
        views.state.retained_from = retained_from;
        assert!(accepted_duplicate_consumed(&views, "command").is_err());
    }
    views.state.retained_from = Some(7);
    assert!(accepted_duplicate_consumed(&views, "command").unwrap());
    views.state.deliveries.insert(
        "command".to_owned(),
        sessions::projector::Delivery::Accepted,
    );
    assert!(!accepted_duplicate_consumed(&views, "command").unwrap());
    views.state.retained_from = None;
    assert!(!accepted_duplicate_consumed(&views, "command").unwrap());
}
