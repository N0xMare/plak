#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;

use crate::runtime::events::reasoning::{
    discard_response_reasoning, seal_reasoning_block, settle_live_thinking,
};
use crate::runtime::test_support::{host_event_sink, pending_reasoning_block, reasoning_block};
use crate::runtime::{MAX_PENDING_REASONING_BYTES, SealedReasoning, TurnProjection};
use crate::sessions::SessionRecord;

/// Reasoning projects only under an admitting bound.
#[test]
fn reasoning_projects_only_under_an_admitting_visibility_bound() {
    use plakous::model::{OutputLocation, ReasoningDelta, ReasoningVisibility};

    let location = OutputLocation::new(0, 0);
    for visibility in [ReasoningVisibility::Visible, ReasoningVisibility::Summary] {
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&reasoning_block(location, visibility));
        assert_eq!(
            projection
                .observe_reasoning_delta(&ReasoningDelta::new(location, b"thinking".to_vec())),
            Some("thinking".to_owned()),
            "{visibility:?}"
        );
        let sealed = projection
            .seal_reasoning(location)
            .expect("admitted reasoning seals");
        assert_eq!(sealed.location_key, "0:0", "{visibility:?}");
        assert_eq!(sealed.text, "thinking", "{visibility:?}");
    }
    for visibility in [
        ReasoningVisibility::Omitted,
        ReasoningVisibility::Redacted,
        ReasoningVisibility::OpaqueReplay,
    ] {
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&reasoning_block(location, visibility));
        assert_eq!(
            projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"secret".to_vec())),
            None,
            "{visibility:?}"
        );
        assert_eq!(projection.seal_reasoning(location), None, "{visibility:?}");
        assert!(projection.seal_all_reasoning().is_empty(), "{visibility:?}");
    }
}

/// A replay that discards its response discards the reasoning that
/// response sealed: the session keeps only the replay's thinking.
#[test]
fn a_discarded_response_never_persists_its_sealed_thinking() {
    use plakous::model::{OutputLocation, ReasoningDelta, ReasoningVisibility};

    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let (events, _rx) = host_event_sink();
    let location = OutputLocation::new(0, 0);
    let mut projection = TurnProjection::<TestExtension>::default();
    for text in ["first try", "replayed"] {
        projection.observe_block_start(&reasoning_block(location, ReasoningVisibility::Visible));
        let _ = projection
            .observe_reasoning_delta(&ReasoningDelta::new(location, text.as_bytes().to_vec()));
        seal_reasoning_block(&mut projection, &events, location);
        if text == "first try" {
            discard_response_reasoning(&mut projection, &events);
        }
    }
    settle_live_thinking(&mut projection, &mut session, &events);
    let thinking = session
        .transcript
        .rows()
        .into_iter()
        .filter_map(|(line, _)| line.as_thinking().cloned())
        .map(|activity| activity.text)
        .collect::<Vec<_>>();
    assert_eq!(thinking, ["replayed"]);
}

#[test]
fn output_limit_discard_then_interrupted_retry_does_not_save_old_thinking() {
    use plakous::model::{OutputLocation, ReasoningDelta, ReasoningVisibility};
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let (events, _rx) = host_event_sink();
    let location = OutputLocation::new(0, 0);
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&reasoning_block(location, ReasoningVisibility::Visible));
    projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"discarded thought"));
    seal_reasoning_block(&mut projection, &events, location);
    discard_response_reasoning(&mut projection, &events);
    // The retry is interrupted before it emits anything. The ordinary
    // terminal flush must not resurrect the discarded response.
    settle_live_thinking(&mut projection, &mut session, &events);
    assert!(
        session
            .transcript
            .rows()
            .into_iter()
            .all(|(line, _)| line.as_thinking().is_none())
    );
}

/// A pending location's buffer is released only by an admitting bound.
#[test]
fn pending_reasoning_buffer_is_released_or_dropped_but_never_leaked() {
    use plakous::model::{
        OutputLocation, ReasoningDelta, ReasoningVisibility, ReasoningVisibilityBound,
    };

    let location = OutputLocation::new(1, 2);

    // Admitting bound releases exactly the buffered bytes, once.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&pending_reasoning_block(location));
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"held".to_vec())),
        None
    );
    assert_eq!(
        projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
            location,
            ReasoningVisibility::Summary
        )),
        Some("held".to_owned())
    );
    let sealed = projection
        .seal_reasoning(location)
        .expect("released pending reasoning seals");
    assert_eq!(sealed.location_key, "1:2");
    assert_eq!(sealed.text, "held");

    // Non-admitting bound drops the buffer without emitting it.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&pending_reasoning_block(location));
    let _ = projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"held".to_vec()));
    assert_eq!(
        projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
            location,
            ReasoningVisibility::Redacted
        )),
        None
    );
    assert_eq!(projection.seal_reasoning(location), None);

    // Overflow drops the buffer and the location stays non-displayable even
    // under a later admitting bound: a partial body is never emitted.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&pending_reasoning_block(location));
    let oversized = vec![b'x'; MAX_PENDING_REASONING_BYTES + 1];
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, oversized)),
        None
    );
    assert_eq!(
        projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
            location,
            ReasoningVisibility::Visible
        )),
        None
    );
    // …and every delta after that bound stays suppressed. Re-admitting the
    // location would emit the tail of a body whose head was dropped, which
    // The transcript contract forbids for the remainder of the block.
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"tail".to_vec())),
        None
    );
    assert_eq!(projection.seal_reasoning(location), None);

    // A bound arriving after an already-emitting admitted block does not
    // reset it: the block still seals.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&reasoning_block(location, ReasoningVisibility::Visible));
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"shown".to_vec())),
        Some("shown".to_owned())
    );
    assert_eq!(
        projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
            location,
            ReasoningVisibility::Visible
        )),
        None
    );
    let sealed = projection
        .seal_reasoning(location)
        .expect("already-emitting admitted block still seals");
    assert_eq!(sealed.location_key, "1:2");
    assert_eq!(sealed.text, "shown");

    // An unbound pending location at response completion emits nothing.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&pending_reasoning_block(location));
    let _ = projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"held".to_vec()));
    assert_eq!(projection.seal_all_reasoning(), [] as [SealedReasoning; 0]);
    assert!(projection.reasoning.is_empty());

    // A delta for a location that never started is non-admitting.
    let mut projection = TurnProjection::<TestExtension>::default();
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"stray".to_vec())),
        None
    );
}
