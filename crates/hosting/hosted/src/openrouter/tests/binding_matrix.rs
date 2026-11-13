//! Production gateway construction from fixed synthetic catalog facts, without I/O.

use super::*;
use crate::agent::rendered_prompts::suite::{self as prompts, Occupancy, RouteSpec};

pub(crate) fn binding_matrix_facts(
    spec: &RouteSpec,
    wire: ResolvedProtocol,
    extensions: &crate::tools::ProductToolExtensions,
    workspace: &Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let account = "openrouter".parse()?;
    let key = PiHubOpenRouterCredentials.source(&account)?;
    let clock = chat_review_clock();
    let route = plakous_openrouter::OpenRouterChatRoute::new_with_retention(
        key.clone(),
        OpenRouterRetentionPolicy::gateway_default(),
    )?
    .with_plak_attribution()?
    .with_clock(clock.clone());
    let client = OpenRouterChatClient::connect_with_retention(
        key,
        OpenRouterRetentionPolicy::gateway_default(),
    )?
    .with_plak_attribution()?;
    let parameters = serde_json::json!(["tools", "tool_choice", "reasoning", "max_tokens"]);
    let models = serde_json::json!({"data":[{"id":spec.model,"canonical_slug":spec.model,
        "context_length":163_840,"supported_parameters":parameters,
        "reasoning":{"supported_efforts":["medium"],"default_enabled":false,"supports_max_tokens":false,"mandatory":false}}],
        "links":{},"total_count":1});
    let endpoints = serde_json::json!({"data":{"id":spec.model,"endpoints":[endpoint_json(AdmissionProbe {
        has_reasoning_param:true,tool_choice_auto:true,max_completion_tokens:Some(65_536),max_prompt_tokens:Some(131_072),
    }, "provider/synthetic-model")]}});
    let evidence = route.fixture_model_evidence(spec.model, &models, &endpoints)?;
    let registry = prompts::registry_for(spec, extensions);
    let names = extensions
        .names()
        .map(plakous::model::ToolName::as_str)
        .collect::<Vec<_>>();
    let effort = Some(ReasoningEffort::Medium);
    let bound = openrouter_output_bound(&evidence, wire, true)?;
    let behavior = openrouter_behavior(
        reasoning_disposition(wire, effort, &evidence)?,
        spec.composition,
        wire,
        &registry,
        &names,
        OutputBoundDisposition::DerivedFromRoute(bound),
    )?;
    let maximum = openrouter_maximum_projected_input(&evidence, wire, true, bound)?;
    let context = match wire {
        ResolvedProtocol::Chat => {
            client.custom_context_contract(evidence.context_length(), maximum)?
        }
        ResolvedProtocol::Responses => {
            client.custom_responses_context_contract(evidence.context_length(), maximum)?
        }
        ResolvedProtocol::Messages => {
            client.custom_messages_context_contract(evidence.context_length(), maximum)?
        }
    };
    let http = plakous_http::Client::new_http1(plakous_http::Limits::for_streaming_inference())?;
    let configured = match wire {
        ResolvedProtocol::Chat => plakous_openrouter::OpenRouterChatLanguageModel::configured_gateway_managed_behavior(route, evidence, context, behavior, http)?,
        ResolvedProtocol::Responses => plakous_openrouter::OpenRouterResponsesLanguageModel::configured_gateway_managed_behavior(route, evidence, context, behavior, http)?,
        ResolvedProtocol::Messages => plakous_openrouter::OpenRouterMessagesLanguageModel::configured_gateway_managed_behavior(route, evidence, context, behavior, http)?,
    };
    let worker = workspace.join("uninvoked-gateway-worker");
    std::fs::write(&worker, b"offline binding fixture artifact")?;
    let workers = PiHubWorkers(worker);
    let runtime = crate::agent::build::api::runtime_binding(
        &crate::agent::build::api::ApiRuntimeSpec {
            runner_services: None,
            job_config: plakous::tools::JobConfig::default(),
            account,
            sidecars: &workers,
            product_home: None,
            literal_model: spec.model,
            effort,
            composition: spec.composition,
            wire,
            workspace_root: workspace,
            cwd: workspace,
            plan_state_authority: prompts::catalog_authority(spec),
            opening_durable_session: false,
            tool_fence: if spec.occupancy == Occupancy::Child {
                crate::agent::SessionToolFence::OccupancyReadOnly
            } else {
                crate::agent::SessionToolFence::Full
            },
            scheduled_compaction: false,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_extensions: extensions,
            telemetry: None,
            spawned_child: spec.occupancy == Occupancy::Child,
        },
        configured,
        registry,
        clock,
    )?;
    Ok(runtime.inspection_facts()?)
}
