use super::with_content_filter_guidance;
use std::collections::BTreeMap;
use std::error::Error;
use std::num::NonZeroU32;
use std::sync::Arc;

use plakous_agent::{AgentBuilder, AgentRequestPolicy};
use plakous_model::*;
use plakous_test_support::{ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep};
use plakous_tools::{ToolRegistryBuilder, WorkspacePolicy};

const FILTER_MESSAGE: &str =
    "stream disconnected before completion: Incomplete response returned, reason: content_filter";

fn descriptor(observed: Timestamp) -> Result<ResolvedModelDescriptor, Box<dyn Error>> {
    let features = BTreeMap::from([(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            FactProvenance::new(FactSource::TestFixture, observed, None),
        ),
    )]);
    Ok(crate::test_model::with_context_contract(
        ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("test.protocol")?,
            RouteId::new("test.route")?,
            ModelId::new("test.model")?,
            None,
            Some(ModelFamilyId::new("test.family")?),
            ModelAdapterProfileId::new("test.profile")?,
            CapabilitySet::new(features, BTreeMap::new()),
        ),
        observed,
    )?)
}

fn assert_guidance(items: ConversationItems<'_>, instruction: &str) {
    let notes: Vec<_> = items
        .iter()
        .filter(|item| matches!(item.blocks(), [ContentBlock::Text(text)] if text.text() == instruction))
        .collect();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].actor(), Actor::User);
    assert_eq!(notes[0].provenance(), Provenance::Instruction);
    assert!(matches!(notes[0].blocks(), [ContentBlock::Text(text)] if text.text() == instruction));
}

fn scenarios(second_filter: bool) -> Result<[ModelScenario; 2], Box<dyn Error>> {
    let filter = || {
        ScenarioStep::Error(ModelError::new(
            ModelErrorKind::ContentFilteredIncomplete,
            FILTER_MESSAGE,
        ))
    };
    let digest = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"scripted-response"])?;
    let next = if second_filter {
        vec![filter()]
    } else {
        let item = ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(7))?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new(
                "A permitted alternative.",
                None,
            )?)],
            Provenance::ModelOutput,
        )?;
        vec![
            ScenarioStep::Emit(ModelEvent::ResponseStarted(ResponseStarted::new(
                None, digest,
            ))),
            ScenarioStep::Emit(ModelEvent::ResponseCompleted(ModelResponse::new(
                vec![item],
                FinishDetails::new(FinishReason::Stop, None),
                None,
                None,
                digest,
            ))),
        ]
    };
    Ok([
        ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![filter()],
        ),
        ModelScenario::new(ExpectedRequest::Any, ScenarioStart::Immediate, next),
    ])
}

#[tokio::test]
async fn sampling_filter_retries_with_guidance_and_second_filter_is_terminal_policy()
-> Result<(), Box<dyn Error>> {
    for second_filter in [false, true] {
        let observed = Timestamp::new(1, 0)?;
        let descriptor = descriptor(observed)?;
        let authority = crate::test_model::runtime_authority(&descriptor, observed)?;
        let inner = Arc::new(crate::test_model::scripted_model(
            descriptor.clone(),
            scenarios(second_filter)?,
            observed,
        )?);
        let instruction = plakous_prompts::content_filter::render(None);
        let model = with_content_filter_guidance(Arc::clone(&inner) as _, None);
        let output_bound = NonZeroU32::new(128).ok_or("nonzero scripted output bound")?;
        let configured = crate::test_model::configured_model(
            model,
            descriptor,
            authority,
            std::iter::empty(),
            LocalDurabilityDisposition::Ephemeral,
            OutputBoundDisposition::Enforced(output_bound),
        )?;
        let workspace_root = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(workspace_root.path(), ".")?);
        let agent = AgentBuilder::new(
            configured,
            ToolRegistryBuilder::default().freeze(),
            workspace,
        )
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(Some(output_bound), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .identity_sources(
            Arc::new(FixedClock::new(observed)),
            Arc::new(SystemIdGenerator),
        )
        .build()?;
        let result = agent
            .prompt("Offer a permitted alternative.")
            .await?
            .result()
            .await?;
        if second_filter {
            let error = result
                .error()
                .ok_or("second filter completed unexpectedly")?;
            assert_eq!(error.model_kind(), Some(ModelErrorKind::Policy));
            assert_eq!(error.message(), FILTER_MESSAGE);
        } else {
            assert!(
                result.response().is_some(),
                "filtered retry failed: {}",
                result
                    .error()
                    .map_or("missing response", |error| error.message())
            );
        }
        assert_guidance(result.conversation().items(), &instruction);
        agent.shutdown().await?;
        let requests = inner.captured_plans();
        assert_eq!(requests.len(), 2, "exactly one sampling retry");
        assert_eq!(inner.remaining_scenarios(), 0);
        assert!(
            !requests[0]
                .request()
                .input()
                .items()
                .iter()
                .any(|item| item.actor() == Actor::User
                    && item.provenance() == Provenance::Instruction)
        );
        assert_guidance(requests[1].request().input().items(), &instruction);
    }
    Ok(())
}
