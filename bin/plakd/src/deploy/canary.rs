use std::io::Write;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous::agent::{Agent, AgentBuilder, AgentConfig, AgentRequestPolicy, TurnResultOutcome};
use plakous::application::ApplicationExecutor;
use plakous::model::{
    CapabilityRequirement, GenerationLimits, LocalDurabilityDisposition, ModelId,
    OutputBoundDisposition, ParallelToolDisposition, ProcessingTierDisposition, ReasoningEffort,
    ReasoningPolicy, SelectedProviderConfiguration, SessionLineage, ToolChoice,
};
use plakous::subscriptions::{
    NativeProvider, NativeToolMode, ResolvedProvider, SubscriptionRequestLimits,
};
use plakous::tools::{ToolRegistryBuilder, WorkspacePolicy};
use plakous_hosted::account::{AccountRef, Vertical};

use crate::commands::application::PlakousApplication;
use crate::product_config::ProductCompositionSelection;
use crate::shell::services::Services;
use crate::{CliFailure, CliResult, ProcessOutcome};

const DEADLINE: Duration = Duration::from_secs(90);
const CODEX_DEADLINE: Duration = Duration::from_secs(45);
const CLEANUP_BOUND: Duration = Duration::from_secs(10);
const OUTPUT_TOKENS: NonZeroU32 = NonZeroU32::new(16).expect("nonzero canary limit");
const PROMPT: &str = "Reply with OK.";
const HTTP_REFUSAL: &str = "plakd-deploy-canary-upstream-http ";

mod local;

pub(super) fn class(kind: &str) -> &'static str {
    adapter(kind).0
}

pub(super) fn pass_line(kind: &str) -> &'static str {
    adapter(kind).1
}

fn adapter(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "codex" => (
            "locally_bounded",
            "plakd-deploy-canary-locally-bounded-pass\n",
        ),
        "openai" | "anthropic" | "xai-api" | "openrouter" => (
            "provider_capped_api_v1",
            "plakd-deploy-canary-provider-capped-api-pass\n",
        ),
        _ => ("bounded_cli_v1", "plakd-deploy-canary-pass\n"),
    }
}

pub(super) fn refused_status(raw: &[u8]) -> Option<u16> {
    let code = std::str::from_utf8(raw)
        .ok()?
        .strip_prefix(HTTP_REFUSAL)?
        .strip_suffix('\n')?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    code.parse()
        .ok()
        .filter(|status| (300..=599).contains(status))
}

pub(crate) fn capabilities(stdout: &mut dyn Write) -> CliResult<ProcessOutcome> {
    writeln!(stdout, "plakd-deploy-canary-v1")
        .and_then(|()| {
            for vertical in Vertical::ALL {
                if vertical.require().is_ok() {
                    writeln!(stdout, "{}", vertical.as_str())?;
                }
            }
            Ok(())
        })
        .map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

#[derive(Clone, Debug, Eq, PartialEq, clap::Args)]
pub(crate) struct Options {
    #[arg(long, conflicts_with_all = ["preflight", "account", "model", "effort", "protocol"])]
    pub(crate) capabilities: bool,
    #[arg(long)]
    preflight: bool,
    #[arg(long, required_unless_present = "capabilities")]
    account: Option<String>,
    #[arg(long, required_unless_present = "capabilities")]
    model: Option<String>,
    #[arg(long, value_parser = ["low", "none"])]
    effort: Option<String>,
    #[arg(long)]
    protocol: Option<String>,
}

fn selection(
    provider: NativeProvider,
    model: &str,
    effort: Option<ReasoningEffort>,
    mode: Option<NativeToolMode>,
) -> CliResult<SelectedProviderConfiguration> {
    let model =
        ModelId::new(model.to_owned()).map_err(|_| CliFailure::new("invalid canary model"))?;
    let selected =
        plakous_hosted::agent::selected_coding_provider_configuration_with_processing_tier(
            provider,
            model,
            effort,
            mode,
            ProductCompositionSelection::Direct,
            ProcessingTierDisposition::Standard,
        )?;
    let mut requested = selected.behavior().clone();
    requested.tools.clear();
    requested.hosted_tools.clear();
    requested.tool_choice = ToolChoice::None;
    requested.parallel_tools = ParallelToolDisposition::Disabled;
    requested.output_bound = output_bound(provider);
    requested.local_durability = LocalDurabilityDisposition::Ephemeral;
    SelectedProviderConfiguration::new(selected.route().clone(), requested)
        .map_err(|_| CliFailure::new("route did not admit bounded canary behavior"))
}

fn output_bound(provider: NativeProvider) -> OutputBoundDisposition {
    if provider == NativeProvider::Codex {
        OutputBoundDisposition::NoRequestBound
    } else {
        OutputBoundDisposition::Enforced(OUTPUT_TOKENS)
    }
}

fn effort(value: &str) -> ReasoningEffort {
    if value == "low" {
        ReasoningEffort::Low
    } else {
        ReasoningEffort::None
    }
}

fn limits(duration: Duration) -> CliResult<SubscriptionRequestLimits> {
    SubscriptionRequestLimits::new(1, 4, 1, 16_384, Some(duration), 0)
        .map_err(|_| CliFailure::new("invalid canary request limits"))
}

pub(crate) fn run(
    services: &Services,
    options: Options,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let started = Instant::now();
    let account: AccountRef = options
        .account
        .as_deref()
        .ok_or_else(|| CliFailure::new("missing canary account"))?
        .parse()?;
    account.vertical().require()?;
    let duration = if account.vertical() == Vertical::Codex {
        CODEX_DEADLINE
    } else {
        DEADLINE
    };
    let deadline = started + duration;
    if options.protocol.is_some() {
        return Err(CliFailure::new(
            "a deploy canary uses its exact enrolled wire, not a protocol override",
        ));
    }
    let model = options
        .model
        .as_deref()
        .ok_or_else(|| CliFailure::new("missing canary model"))?;
    let executor = ApplicationExecutor::new()
        .map_err(|_| CliFailure::new("cannot construct the canary executor"))?;
    let requested_effort = options.effort.as_deref().map(effort);
    if account.vertical().is_api() {
        let refusal = run_api(
            services,
            &executor,
            &account,
            model,
            requested_effort,
            options.preflight,
            deadline,
        )?;
        if let Some(status) = refusal {
            writeln!(stdout, "{HTTP_REFUSAL}{status}")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            return Err(api_failure(plakous::gateway::ApiCanaryFailure::Upstream(
                status,
            )));
        }
    } else {
        run_native(
            services,
            &executor,
            &account,
            model,
            requested_effort,
            options.preflight,
            deadline,
        )?;
    }
    if !options.preflight {
        stdout
            .write_all(pass_line(account.vertical().as_str()).as_bytes())
            .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

fn run_native(
    services: &Services,
    executor: &ApplicationExecutor,
    account: &AccountRef,
    model: &str,
    requested_effort: Option<ReasoningEffort>,
    preflight: bool,
    deadline: Instant,
) -> CliResult<()> {
    let provider = match account.vertical() {
        Vertical::Grok => NativeProvider::Xai,
        Vertical::Claude => NativeProvider::Claude,
        Vertical::Codex => NativeProvider::Codex,
        _ => return Err(CliFailure::new("not a subscription canary account")),
    };
    let _generation = crate::gateway::deployment_generation(services, account.as_str())?;
    let backend = PlakousApplication::new(
        services.home().state()?.to_path_buf(),
        None,
        None,
        services.custody()?,
    )?;
    let subscriptions = backend.subscriptions_for(account)?;
    let limits = limits(if provider == NativeProvider::Codex {
        CODEX_DEADLINE
    } else {
        DEADLINE
    })?;
    let (resolved, selected_effort) = executor
        .block_on(async {
            tokio::time::timeout_at(deadline.into(), async {
                let (effort, mode) = if provider == NativeProvider::Codex {
                    let catalog = subscriptions.model_catalog(provider).await.map_err(|_| {
                        CliFailure::new(
                            "Codex canary could not read the authenticated model catalog",
                        )
                    })?;
                    let row = catalog
                        .models()
                        .iter()
                        .find(|row| {
                            row.exact_id() == model
                                && row.supports_streaming()
                                && row.unavailable_reason().is_none()
                        })
                        .ok_or_else(|| {
                            CliFailure::new(
                                "Codex canary requires an exact admitted streaming model",
                            )
                        })?;
                    let lowest = lowest_effort(row.reasoning_efforts())?;
                    if lowest != requested_effort {
                        return Err(CliFailure::new(
                            "pin the Codex model's lowest admitted effort in inventory",
                        ));
                    }
                    (lowest, row.native_tool_mode())
                } else {
                    (requested_effort, None)
                };
                let selected = selection(provider, model, effort, mode)?;
                let resolved = subscriptions
                    .resolve_turn_scoped_provider(selected, limits, SessionLineage::Root)
                    .await
                    .map_err(|_| CliFailure::new("route refused bounded canary admission"))?;
                Ok::<_, CliFailure>((resolved, effort))
            })
            .await
        })
        .map_err(|_| CliFailure::new("cannot drive the canary executor"))?
        .map_err(|_| CliFailure::new("canary admission deadline expired"))??;
    let behavior = resolved.configuration().effective().behavior();
    if !behavior.tools.is_empty()
        || !behavior.hosted_tools.is_empty()
        || behavior.tool_choice != ToolChoice::None
        || behavior.local_durability != LocalDurabilityDisposition::Ephemeral
        || behavior.output_bound != output_bound(provider)
    {
        return Err(CliFailure::new(
            "effective canary policy did not retain its tool and output fences",
        ));
    }
    if !preflight {
        drive(
            services,
            executor,
            account,
            &resolved,
            selected_effort,
            deadline,
        )?;
    }
    Ok(())
}

fn lowest_effort(available: &[ReasoningEffort]) -> CliResult<Option<ReasoningEffort>> {
    for candidate in [
        ReasoningEffort::None,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
        ReasoningEffort::Ultra,
    ] {
        if available.contains(&candidate) {
            return Ok(Some(candidate));
        }
    }
    if available.is_empty() {
        Ok(None)
    } else {
        Err(CliFailure::new(
            "Codex canary has no recognized admitted reasoning effort",
        ))
    }
}

fn run_api(
    services: &Services,
    executor: &ApplicationExecutor,
    account: &AccountRef,
    model: &str,
    requested_effort: Option<ReasoningEffort>,
    preflight: bool,
    deadline: Instant,
) -> CliResult<Option<u16>> {
    let custody = services.custody()?;
    let gateway = custody
        .gateway()
        .ok_or_else(|| CliFailure::new("deploy requires an enrolled gateway"))?;
    custody
        .verify_holder(services.home().state()?)
        .map_err(|error| CliFailure::new(error.into_message()))?;
    executor
        .block_on(async {
            let preparation = gateway.prepare_api_canary(
                account.as_str(),
                model,
                requested_effort == Some(ReasoningEffort::Low),
            );
            let request = tokio::time::timeout_at(deadline.into(), preparation)
                .await
                .map_err(|_| CliFailure::new("API canary admission deadline expired"))?
                .map_err(api_failure)?;
            if preflight {
                return Ok(None);
            }
            let _admission = crate::carry::upgrade::admission(services.paths()?, false)?;
            let workspace = workspace()?;
            archive(
                services,
                account,
                model,
                requested_effort,
                workspace.path().to_owned(),
            )?;
            if Instant::now() >= deadline {
                return Err(CliFailure::new(
                    "API canary deadline expired before inference",
                ));
            }
            // The exchange carries its own deadline; leave time for it to
            // close and join the response on a refusal.
            match request.run(deadline).await {
                Ok(()) => Ok(None),
                Err(plakous::gateway::ApiCanaryFailure::Upstream(status)) => Ok(Some(status)),
                Err(error) => Err(api_failure(error)),
            }
        })
        .map_err(|_| CliFailure::new("cannot drive the API canary executor"))?
}

fn api_failure(error: plakous::gateway::ApiCanaryFailure) -> CliFailure {
    CliFailure::new(error.to_string())
}

fn drive(
    services: &Services,
    executor: &ApplicationExecutor,
    account: &AccountRef,
    resolved: &ResolvedProvider,
    effort: Option<ReasoningEffort>,
    deadline: Instant,
) -> CliResult<()> {
    let _admission = crate::carry::upgrade::admission(services.paths()?, false)?;
    let workspace = workspace()?;
    let root = workspace
        .path()
        .canonicalize()
        .map_err(|_| CliFailure::new("cannot resolve the canary workspace"))?;
    let policy = WorkspacePolicy::host(&root, ".")
        .map_err(|_| CliFailure::new("invalid canary workspace"))?;
    let reasoning = effort.map_or(ReasoningPolicy::Disabled, |value| {
        ReasoningPolicy::Enabled {
            effort: Some(value),
            requirement: CapabilityRequirement::Required,
        }
    });
    let locally_bounded = account.vertical() == Vertical::Codex;
    let generation = GenerationLimits::new((!locally_bounded).then_some(OUTPUT_TOKENS), Vec::new())
        .map_err(|_| CliFailure::new("invalid canary output cap"))?;
    let builder = AgentBuilder::new(
        resolved.configured_model(),
        ToolRegistryBuilder::default().freeze(),
        Arc::new(policy),
    )
    .request_policy(AgentRequestPolicy::new(generation, reasoning))
    .config(
        AgentConfig::default()
            .with_max_model_rounds(1)
            .with_scheduled_compaction_enabled(false),
    );
    archive(
        services,
        account,
        resolved.selected_model().as_str(),
        effort,
        root,
    )?;
    executor
        .block_on(async {
            if Instant::now() >= deadline {
                return Err(CliFailure::new("canary deadline expired before admission"));
            }
            let agent = builder
                .build()
                .map_err(|_| CliFailure::new("cannot start the canary runtime"))?;
            join_after(deadline, prompt(&agent, locally_bounded), || {
                agent.shutdown()
            })
            .await
        })
        .map_err(|_| CliFailure::new("cannot drive the canary executor"))?
}

fn workspace() -> CliResult<tempfile::TempDir> {
    tempfile::tempdir().map_err(|_| CliFailure::new("cannot create a private canary workspace"))
}

fn archive(
    services: &Services,
    account: &AccountRef,
    model: &str,
    effort: Option<ReasoningEffort>,
    root: std::path::PathBuf,
) -> CliResult<()> {
    let mut record = plakous_hosted::sessions::SessionRecord::new(root);
    record.archived = true;
    record.title = Some("Deployment canary".to_owned());
    if !account.vertical().is_api() {
        record.account = Some(account.to_string());
        record.model = Some(model.to_owned());
        record.effort = plakous_hosted::platform::effort_names(effort.as_slice())
            .first()
            .map(|s| (*s).to_owned());
        record.composition = ProductCompositionSelection::Direct;
    }
    // API audit rows are marker-only: there is no durable API target to reopen.
    // The deploy and gateway evidence retain the selected account.
    services
        .session_records()?
        .save(&mut record)
        .map_err(|_| CliFailure::new("cannot archive the canary session"))
}

async fn prompt(agent: &Agent, locally_bounded: bool) -> CliResult<()> {
    let turn = agent
        .prompt(PROMPT)
        .await
        .map_err(|_| CliFailure::new("canary prompt was not admitted"))?;
    if locally_bounded {
        return local::cancel_at_bound(turn).await;
    }
    let outcome = turn
        .result()
        .await
        .map_err(|_| CliFailure::new("canary turn did not settle"))?;
    match outcome.outcome() {
        TurnResultOutcome::Completed(_) => Ok(()),
        _ => Err(CliFailure::new("canary turn did not complete")),
    }
}

async fn join_after<C, E>(
    deadline: Instant,
    turn: impl Future<Output = CliResult<()>>,
    cleanup: impl FnOnce() -> C,
) -> CliResult<()>
where
    C: Future<Output = Result<(), E>>,
{
    let verdict = tokio::time::timeout_at(deadline.into(), turn).await;
    tokio::time::timeout(CLEANUP_BOUND, cleanup())
        .await
        .map_err(|_| CliFailure::new("canary cleanup deadline expired"))?
        .map_err(|_| CliFailure::new("canary runtime did not shut down"))?;
    verdict.map_err(|_| CliFailure::new("canary turn deadline expired"))?
}

#[cfg(test)]
mod tests;
