use super::*;
use std::time::Duration;

#[test]
fn sustained_arguments_and_inactivity_have_bounded_content_free_diagnostics() {
    let start = Instant::now();
    let mut progress = StreamProgress::new(start);
    assert!(
        progress
            .deadline_message(start)
            .contains("observed_phase=Waiting")
    );
    for index in 0..3979 {
        progress.observe(
            &crate::ResponsesStreamEventKind::FunctionCallArgumentsDelta {
                output_index: 0,
                item_id: Some("private-id".into()),
                delta: "private-content".into(),
            },
            start + Duration::from_millis(index * 30),
        );
    }
    let active = progress.deadline_message(start + Duration::from_millis(119_352));
    assert!(active.contains("observed_argument_deltas=3979 observed_argument_completions=0"));
    assert!(active.contains("last_observation_age_ms=12"));
    assert!(!active.contains("private"));
    assert!(active.len() < 512);
    assert!(active.contains("authoritative_completion=false"));
    let idle = progress.deadline_message(start + Duration::from_millis(129_340));
    assert!(idle.contains("last_observation_age_ms=10000"));
    progress.observe(
        &crate::ResponsesStreamEventKind::Unknown {
            kind: "private-label".into(),
            native: serde_json::json!({"secret": "private"}),
        },
        start,
    );
    let unknown = progress.deadline_message(start);
    assert!(unknown.contains("observed_phase=Unknown"));
    assert!(!unknown.contains("private"));
}
