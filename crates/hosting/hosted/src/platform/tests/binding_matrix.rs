//! The production API constructor with fixed offline credentials and no dispatch.

use super::*;
use crate::agent::rendered_prompts::suite::{self as prompts, Occupancy, RouteSpec};

pub(crate) async fn binding_matrix_facts(
    provider: PlatformProvider,
    route: &RouteSpec,
    extensions: &crate::tools::ProductToolExtensions,
    workspace: &std::path::Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let worker = workspace.join("uninvoked-worker");
    std::fs::write(&worker, b"offline binding fixture artifact")?;
    let workers = FixtureWorkers(worker);
    let spec = ApiRuntimeSpec {
        runner_services: None,
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account().parse()?,
        sidecars: &workers,
        product_home: None,
        literal_model: route.model,
        effort: Some(ReasoningEffort::Medium),
        composition: route.composition,
        wire: provider.protocol(),
        workspace_root: workspace,
        cwd: workspace,
        plan_state_authority: prompts::catalog_authority(route),
        opening_durable_session: false,
        tool_fence: if route.occupancy == Occupancy::Child {
            crate::agent::SessionToolFence::OccupancyReadOnly
        } else {
            crate::agent::SessionToolFence::Full
        },
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: extensions,
        telemetry: None,
        spawned_child: route.occupancy == Occupancy::Child,
    };
    let runtime = if provider == PlatformProvider::Anthropic {
        let registry = crate::agent::coding_tool_registry_with_extensions(
            spec.plan_state_authority,
            spec.composition,
            false,
            extensions,
        )?;
        let catalog = bundled_catalog(provider, "offline binding fixture");
        let row = catalog
            .admit(route.model, spec.effort)
            .map_err(CliFailure::from)?;
        let window = row.window(ApiWindow::Working);
        let extras = extensions.names().map(ToolName::as_str).collect::<Vec<_>>();
        let tools = crate::agent::build::api::configured_tools(
            crate::coding_routes::platform_presentation(provider),
            spec.composition,
            spec.wire,
            &registry,
            &extras,
        )?;
        let clock: Arc<dyn plakous::model::Clock> = Arc::new(plakous::model::FixedClock::new(
            plakous::model::Timestamp::new(1, 0)?,
        ));
        let configured = plakous::anthropic::AnthropicLanguageModel::configured_with_fixture_clock(
            Credentials("binding-fixture-key").anthropic(&spec.account)?,
            &spec.account.source_authority("plak/anthropic"),
            row.anthropic
                .as_ref()
                .ok_or("Anthropic fixture facts absent")?,
            &row.binding_evidence_for(provider, window),
            platform_behavior(provider, ReasoningEffort::Medium, row, false, tools)?,
            NonZeroU64::new(window).ok_or("invalid fixture window")?,
            clock.clone(),
        )?;
        crate::agent::build::api::runtime_binding(&spec, configured, registry, clock)?
    } else {
        construct_platform_runtime(provider, &Credentials("binding-fixture-key"), &spec, None)
            .await?
    };
    Ok(runtime.inspection_facts()?)
}
