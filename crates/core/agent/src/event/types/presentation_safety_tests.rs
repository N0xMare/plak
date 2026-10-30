use plakous_model::{
    BlockStart, BlockStarted, ContentBlock, EntryId, ModelEvent, OutputLocation,
    ReasoningVisibility, TextBlock,
};
use uuid::Uuid;

use super::{AgentEventKind, EventVisibility};

#[test]
fn stream_activity_is_not_a_presentation_event() {
    let event = ModelEvent::StreamActivity(plakous_model::ModelStreamActivity::default());
    assert!(super::presentation_safe_model_event(&event).is_none());
}

#[test]
fn prompt_cache_diagnosis_is_not_a_presentation_event() {
    let event = ModelEvent::PromptCacheDiagnosis(plakous_model::PromptCacheDiagnosis {
        reason: plakous_model::PromptCacheMissReason::ToolsChanged,
        missed_input_tokens: Some(4),
    });
    assert!(super::presentation_safe_model_event(&event).is_none());
}

#[test]
fn prepared_request_names_are_content_only() -> Result<(), Box<dyn std::error::Error>> {
    let event = AgentEventKind::RequestPrepared {
        input_parts: None,
        occupied_tokens: Some(23),
        prepared_input_upper_bound: Some(42),
        tool_names: vec![plakous_model::ToolName::new("read_file")?],
    };
    let AgentEventKind::RequestPrepared { tool_names, .. } =
        event.projected(EventVisibility::Content)
    else {
        panic!("content projection must keep the request shape");
    };
    assert_eq!(tool_names.len(), 1);
    let AgentEventKind::RequestPrepared {
        tool_names,
        occupied_tokens,
        ..
    } = event.projected(EventVisibility::Metadata)
    else {
        panic!("metadata projection must keep the request shape");
    };
    assert_eq!(tool_names, [] as [plakous_model::ToolName; 0]);
    assert_eq!(occupied_tokens, Some(23));
    Ok(())
}

fn started(block: BlockStart) -> Result<ModelEvent, Box<dyn std::error::Error>> {
    Ok(ModelEvent::BlockStarted(BlockStarted::new(
        EntryId::try_from(Uuid::from_u128(7))?,
        OutputLocation::new(0, 0),
        block,
    )))
}

#[test]
fn a_content_view_retains_transient_history_projection_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let identity = plakous_model::Digest::sha256(
        plakous_model::DigestDomain::HistoryProjection,
        1,
        &[b"transition"],
    )?;
    let event = AgentEventKind::model(ModelEvent::Diagnostic(
        plakous_model::ModelDiagnostic::historical_projection(identity, "history projected"),
    ));
    let content = event.projected(EventVisibility::Content);
    assert!(matches!(
        content.model_event(),
        Some(ModelEvent::Diagnostic(diagnostic))
            if diagnostic.history_projection_identity() == Some(identity)
    ));
    assert!(
        event
            .projected(EventVisibility::Metadata)
            .model_event()
            .is_none()
    );
    Ok(())
}

/// The host attributes reasoning deltas to the visibility declared at
/// `BlockStarted`, so a Content view must carry the start
/// and its visibility, without any replay envelope.
#[test]
fn a_content_view_keeps_reasoning_block_starts_and_their_visibility()
-> Result<(), Box<dyn std::error::Error>> {
    let event = AgentEventKind::model(started(BlockStart::Reasoning {
        visibility: ReasoningVisibility::Summary,
        replay: None,
    })?)
    .projected(EventVisibility::Content);
    let Some(ModelEvent::BlockStarted(kept)) = event.model_event() else {
        panic!("block start is presentation-safe: {event:?}");
    };
    assert_eq!(
        kept.block(),
        &BlockStart::Reasoning {
            visibility: ReasoningVisibility::Summary,
            replay: None,
        }
    );
    let pending = AgentEventKind::model(started(BlockStart::ReasoningPending { replay: None })?)
        .projected(EventVisibility::Content);
    assert!(matches!(
        pending.model_event(),
        Some(ModelEvent::BlockStarted(kept))
            if kept.block() == &BlockStart::ReasoningPending { replay: None }
    ));
    Ok(())
}

#[test]
fn a_content_view_never_carries_complete_blocks_and_metadata_carries_no_model_event()
-> Result<(), Box<dyn std::error::Error>> {
    let complete = started(BlockStart::Complete(ContentBlock::Text(TextBlock::new(
        "done", None,
    )?)))?;
    assert!(
        AgentEventKind::model(complete.clone())
            .projected(EventVisibility::Content)
            .model_event()
            .is_none()
    );
    assert!(
        AgentEventKind::model(complete)
            .projected(EventVisibility::Metadata)
            .model_event()
            .is_none()
    );
    Ok(())
}
