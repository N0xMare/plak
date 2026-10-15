//! `OpenRouter` protocol evidence exercised exclusively through Responses-family hosts.

use std::error::Error;
use std::fs;
use std::io;
use std::num::NonZeroU32;
use std::path::Path;
use std::str::FromStr as _;

use plakous_model::{
    Actor, AttemptId, ContentBlock, ConversationItem, ConversationView, Digest, DigestDomain,
    EffectiveProviderConfigurationId, EntryId, GenerationLimits, ModelFamilyId, ModelId,
    ModelRequest, ProtocolId, Provenance, RequestMetadata, RunId, SelectedProviderConfigurationId,
    TextBlock, Timestamp, TurnId,
};
use plakous_responses::ResponsesStreamLimits;
use plakous_responses_dialect::{
    ResponsesFamilySseDecoder, ResponsesFamilyStreamError, ResponsesRequestConfig,
    ResponsesRequestPolicy, ResponsesStreamDialectData, ResponsesStreamEventKind,
    lower_responses_request,
};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const MODEL: &str = "openai/o4-mini";

fn configuration_ids() -> TestResult<(
    SelectedProviderConfigurationId,
    EffectiveProviderConfigurationId,
)> {
    Ok((
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"openrouter-shared-host-selected"],
        )?)?)?,
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"openrouter-shared-host-effective"],
        )?)?)?,
    ))
}

fn request() -> Result<ModelRequest, Box<dyn Error>> {
    let (selected, effective) = configuration_ids()?;
    let input = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000010")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "Route exactly once.",
            None,
        )?)],
        Provenance::UserInput,
    )?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![input])?,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000001")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000002")?,
            AttemptId::from_str("00000000-0000-4000-8000-000000000003")?,
            Timestamp::new(1_785_326_400, 0)?,
        ),
    )
    .limits(GenerationLimits::new(
        Some(NonZeroU32::new(128).ok_or_else(|| io::Error::other("literal must be nonzero"))?),
        Vec::new(),
    )?)
    .build()?)
}

fn decoder() -> ResponsesFamilySseDecoder {
    ResponsesFamilySseDecoder::new(
        ResponsesStreamDialectData::openrouter(MODEL),
        ResponsesStreamLimits::default(),
    )
}

fn created(sequence: u64) -> String {
    format!(
        "data: {{\"type\":\"response.created\",\"sequence_number\":{sequence},\"response\":{{\"id\":\"resp_exact\",\"model\":\"{MODEL}\",\"status\":\"in_progress\",\"output\":[]}}}}\n\n"
    )
}

fn terminal(sequence: u64) -> String {
    format!(
        "data: {{\"type\":\"response.done\",\"sequence_number\":{sequence},\"response\":{{\"id\":\"resp_exact\",\"model\":\"{MODEL}\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}},\"incomplete_details\":null,\"error\":null}}}}\n\n"
    )
}

#[test]
fn gateway_managed_lowering_makes_no_endpoint_or_provider_claim() -> TestResult {
    let request = request()?;
    let protocol = ProtocolId::new("openrouter.responses.v1")?;
    let family = ModelFamilyId::new("openai")?;
    let model = ModelId::new(MODEL)?;
    let lowered = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model,
            ResponsesRequestPolicy::openrouter_stateless(),
            None,
        ),
    )?;

    assert_eq!(
        lowered.body(),
        &json!({
            "model": MODEL,
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Route exactly once."}]
            }],
            "stream": true,
            "store": false,
            "max_output_tokens": 128,
        })
    );
    assert!(lowered.body().get("provider").is_none());
    assert!(!lowered.body().to_string().contains("only"));
    assert!(!lowered.body().to_string().contains("order"));
    Ok(())
}

#[test]
fn openrouter_sse_requires_ordered_typed_terminal_then_done() -> TestResult {
    let mut valid = decoder();
    let stream = format!("{}{}data: [DONE]\n\n", created(10), terminal(11));
    let events = valid.push(stream.as_bytes())?;
    assert!(matches!(
        events.as_slice(),
        [
            ResponsesStreamEventKind::ResponseCreated { .. },
            ResponsesStreamEventKind::ResponseCompleted(_)
        ]
    ));
    assert_eq!(
        valid.finish()?,
        [] as [ResponsesStreamEventKind<
            plakous_responses_dialect::ResponsesResponse,
            plakous_responses_dialect::ResponsesProviderError,
        >; 0]
    );

    let mut missing_done = decoder();
    missing_done.push(format!("{}{}", created(10), terminal(11)).as_bytes())?;
    assert_eq!(
        missing_done.finish(),
        Err(ResponsesFamilyStreamError::DonePolicy)
    );

    let mut done_before_terminal = decoder();
    assert_eq!(
        done_before_terminal.push(b"data: [DONE]\n\n"),
        Err(ResponsesFamilyStreamError::DonePolicy)
    );

    let mut duplicate_sequence = decoder();
    duplicate_sequence.push(created(10).as_bytes())?;
    assert_eq!(
        duplicate_sequence.push(terminal(10).as_bytes()),
        Err(ResponsesFamilyStreamError::InvalidSequence)
    );
    Ok(())
}

#[test]
fn pinned_official_sparse_lifecycle_uses_only_prepared_model_and_completed_items() -> TestResult {
    let mut sparse = decoder();
    let mut fixture = include_bytes!("../fixtures/responses-basic-usage-sparse.sse").to_vec();
    fixture.push(b'\n');
    let events = sparse.push(&fixture)?;
    let response = events
        .iter()
        .find_map(|event| match event {
            ResponsesStreamEventKind::ResponseCompleted(response) => Some(response),
            _ => None,
        })
        .ok_or("sparse fixture omitted completed response")?;
    assert_eq!(response.id(), "resp_exact");
    assert_eq!(response.model(), MODEL);
    assert_eq!(response.output().len(), 1);
    let terminal_native = include_str!("../fixtures/responses-basic-usage-sparse.sse")
        .lines()
        .find_map(|line| line.strip_prefix("data: {\"type\":\"response.done\""))
        .map(|suffix| format!("{{\"type\":\"response.done\"{suffix}"))
        .ok_or("sparse fixture omitted response.done")?;
    let terminal_native: serde_json::Value = serde_json::from_str(&terminal_native)?;
    assert_eq!(response.native(), &terminal_native["response"]);
    assert!(response.native().get("model").is_none());
    assert!(response.native().get("output").is_none());
    assert_eq!(
        sparse.finish()?,
        [] as [ResponsesStreamEventKind<
            plakous_responses_dialect::ResponsesResponse,
            plakous_responses_dialect::ResponsesProviderError,
        >; 0]
    );

    let mut wrong_created_model = decoder();
    assert!(matches!(
        wrong_created_model.push(
            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_exact\",\"model\":\"foreign/model\"}}\n\n"
        ),
        Err(ResponsesFamilyStreamError::Lifecycle(_))
    ));

    let mut explicit_wrong_terminal_model = decoder();
    explicit_wrong_terminal_model
        .push(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_exact\"}}\n\n")?;
    assert_eq!(
        explicit_wrong_terminal_model.push(
            b"data: {\"type\":\"response.done\",\"response\":{\"id\":\"resp_exact\",\"model\":\"foreign/model\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
        ),
        Err(ResponsesFamilyStreamError::InvalidLifecycle)
    );

    let mut incomplete_item = decoder();
    incomplete_item.push(
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_exact\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"response_id\":\"resp_exact\",\"output_index\":0,\"item\":{\"id\":\"msg_exact\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n"
        )
        .as_bytes(),
    )?;
    assert!(matches!(
        incomplete_item.push(
            b"data: {\"type\":\"response.done\",\"response\":{\"id\":\"resp_exact\",\"object\":\"response\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
        ),
        Err(ResponsesFamilyStreamError::Lifecycle(_))
    ));

    let mut reordered_item = decoder();
    reordered_item
        .push(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_exact\"}}\n\n")?;
    assert!(matches!(
        reordered_item.push(
            b"data: {\"type\":\"response.output_item.added\",\"response_id\":\"resp_exact\",\"output_index\":1,\"item\":{\"id\":\"msg_reordered\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n"
        ),
        Err(ResponsesFamilyStreamError::Lifecycle(_))
    ));
    Ok(())
}

#[test]
fn duplicate_json_and_future_declared_events_fail_closed() -> TestResult {
    for frame in [
        format!(
            "data: {{\"type\":\"response.created\",\"type\":\"response.done\",\"response\":{{\"id\":\"resp_exact\",\"model\":\"{MODEL}\"}}}}\n\n"
        ),
        format!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_exact\",\"id\":\"resp_substituted\",\"model\":\"{MODEL}\"}}}}\n\n"
        ),
    ] {
        let mut duplicate = decoder();
        assert_eq!(
            duplicate.push(frame.as_bytes()),
            Err(ResponsesFamilyStreamError::InvalidJson)
        );
    }

    let mut future = decoder();
    future.push(created(1).as_bytes())?;
    assert_eq!(
        future.push(
            b"data: {\"type\":\"response.function_call_arguments.future\",\"sequence_number\":2,\"response_id\":\"resp_exact\",\"output_index\":0}\n\n"
        ),
        Err(ResponsesFamilyStreamError::Unsupported("unknown event"))
    );
    Ok(())
}

#[test]
fn openrouter_has_no_peer_responses_machine_modules() -> TestResult {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for forbidden in [
        "attempt.rs",
        "client.rs",
        "project.rs",
        "request.rs",
        "response.rs",
        "stream.rs",
    ] {
        assert!(
            !source.join(forbidden).exists(),
            "OpenRouter must use the shared Responses host, not {forbidden}"
        );
    }
    let lib = fs::read_to_string(source.join("lib.rs"))?;
    for forbidden in [
        "mod attempt;",
        "mod client;",
        "mod project;",
        "mod request;",
        "mod response;",
        "mod stream;",
    ] {
        assert!(
            !lib.contains(forbidden),
            "OpenRouter must not declare peer module `{forbidden}`"
        );
    }
    Ok(())
}
