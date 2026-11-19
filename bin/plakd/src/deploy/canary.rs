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
use plakous::subscriptions::{NativeProvider, ResolvedProvider, SubscriptionRequestLimits};
use plakous::tools::{ToolRegistryBuilder, WorkspacePolicy};
use plakous_hosted::account::AccountRef;

use crate::commands::application::PlakousApplication;
use crate::product_config::ProductCompositionSelection;
use crate::shell::services::Services;
use crate::{CliFailure, CliResult, ProcessOutcome};

const DEADLINE: Duration = Duration::from_secs(90);
const CLEANUP_BOUND: Duration = Duration::from_secs(10);
const OUTPUT_TOKENS: NonZeroU32 = NonZeroU32::new(16).expect("nonzero canary limit");

pub(crate) fn capabilities(stdout: &mut dyn Write) -> CliResult<ProcessOutcome> {
    writeln!(stdout, "plakd-deploy-canary-v1")
        .and_then(|()| {
            for (name, provider) in [
                ("claude", NativeProvider::Claude),
                ("grok", NativeProvider::Xai),
            ] {
                if plakous_hosted::capabilities::require_native(provider).is_ok() {
                    writeln!(stdout, "{name}")?;
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

fn selection(options: &Options, account: &AccountRef) -> CliResult<SelectedProviderConfiguration> {
    let provider = match account.as_str().split('/').next() {
        Some("grok") => NativeProvider::Xai,
        Some("claude") => NativeProvider::Claude,
        // Codex's OAuth route does not admit a request output bound.
        _ => {
            return Err(CliFailure::new(
                "route has no bounded deploy canary adapter",
            ));
        }
    };
    if options.protocol.is_some() {
        return Err(CliFailure::new(
            "a native deploy canary does not select a wire protocol",
        ));
    }
    let model = options
        .model
        .as_deref()
        .ok_or_else(|| CliFailure::new("missing canary model"))?;
    let model =
        ModelId::new(model.to_owned()).map_err(|_| CliFailure::new("invalid canary model"))?;
    let selected =
        plakous_hosted::agent::selected_coding_provider_configuration_with_processing_tier(
            provider,
            model,
            options.effort.as_deref().map(effort),
            None,
            ProductCompositionSelection::Direct,
            ProcessingTierDisposition::Standard,
        )?;
    let mut requested = selected.behavior().clone();
    requested.tools.clear();
    requested.hosted_tools.clear();
    requested.tool_choice = ToolChoice::None;
    requested.parallel_tools = ParallelToolDisposition::Disabled;
    requested.output_bound = OutputBoundDisposition::Enforced(OUTPUT_TOKENS);
    requested.local_durability = LocalDurabilityDisposition::Ephemeral;
    SelectedProviderConfiguration::new(selected.route().clone(), requested)
        .map_err(|_| CliFailure::new("route did not admit bounded canary behavior"))
}

fn effort(value: &str) -> ReasoningEffort {
    if value == "low" {
        ReasoningEffort::Low
    } else {
        ReasoningEffort::None
    }
}

fn limits() -> CliResult<SubscriptionRequestLimits> {
    SubscriptionRequestLimits::new(1, 4, 1, 16_384, Some(DEADLINE), 0)
        .map_err(|_| CliFailure::new("invalid canary request limits"))
}

pub(crate) fn run(
    services: &Services,
    options: Options,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let deadline = Instant::now() + DEADLINE;
    let account: AccountRef = options
        .account
        .as_deref()
        .ok_or_else(|| CliFailure::new("missing canary account"))?
        .parse()?;
    let selected = selection(&options, &account)?;
    let _generation = crate::gateway::deployment_generation(services, account.as_str())?;
    let backend = PlakousApplication::new(
        services.home().state()?.to_path_buf(),
        None,
        None,
        services.custody()?,
    )?;
    let subscriptions = backend.subscriptions_for(&account)?;
    let limits = limits()?;
    let executor = ApplicationExecutor::new()
        .map_err(|_| CliFailure::new("cannot construct the canary executor"))?;
    let resolved = executor
        .block_on(async {
            tokio::time::timeout_at(
                deadline.into(),
                subscriptions.resolve_turn_scoped_provider(selected, limits, SessionLineage::Root),
            )
            .await
        })
        .map_err(|_| CliFailure::new("cannot drive the canary executor"))?
        .map_err(|_| CliFailure::new("canary admission deadline expired"))?
        .map_err(|_| CliFailure::new("route refused bounded canary admission"))?;
    let behavior = resolved.configuration().effective().behavior();
    if !behavior.tools.is_empty()
        || !behavior.hosted_tools.is_empty()
        || behavior.tool_choice != ToolChoice::None
        || behavior.local_durability != LocalDurabilityDisposition::Ephemeral
        || behavior.output_bound != OutputBoundDisposition::Enforced(OUTPUT_TOKENS)
    {
        return Err(CliFailure::new(
            "effective canary policy did not retain its tool and output fences",
        ));
    }
    if !options.preflight {
        drive(
            services,
            &executor,
            account,
            &resolved,
            options.effort,
            deadline,
        )?;
        writeln!(stdout, "plakd-deploy-canary-pass")
            .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

fn drive(
    services: &Services,
    executor: &ApplicationExecutor,
    account: AccountRef,
    resolved: &ResolvedProvider,
    effort_name: Option<String>,
    deadline: Instant,
) -> CliResult<()> {
    let _admission = crate::carry::upgrade::admission(services.paths()?, false)?;
    let workspace = tempfile::tempdir()
        .map_err(|_| CliFailure::new("cannot create a private canary workspace"))?;
    let root = workspace
        .path()
        .canonicalize()
        .map_err(|_| CliFailure::new("cannot resolve the canary workspace"))?;
    let policy = WorkspacePolicy::host(&root, ".")
        .map_err(|_| CliFailure::new("invalid canary workspace"))?;
    let reasoning = effort_name
        .as_deref()
        .map_or(ReasoningPolicy::Disabled, |value| {
            ReasoningPolicy::Enabled {
                effort: Some(effort(value)),
                requirement: CapabilityRequirement::Required,
            }
        });
    let generation = GenerationLimits::new(Some(OUTPUT_TOKENS), Vec::new())
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
    let mut record = plakous_hosted::sessions::SessionRecord::new(root);
    record.archived = true;
    record.account = Some(account.to_string());
    record.model = Some(resolved.selected_model().as_str().to_owned());
    record.title = Some("Deployment canary".to_owned());
    record.effort = effort_name;
    record.composition = ProductCompositionSelection::Direct;
    // Commit the archived audit row before any runtime admission. The agent is
    // ephemeral: even SIGKILL cannot leave a runtime branch for the host to adopt.
    services
        .session_records()?
        .save(&mut record)
        .map_err(|_| CliFailure::new("cannot archive the canary session"))?;
    executor
        .block_on(async {
            if Instant::now() >= deadline {
                return Err(CliFailure::new("canary deadline expired before admission"));
            }
            let agent = builder
                .build()
                .map_err(|_| CliFailure::new("cannot start the canary runtime"))?;
            join_after(deadline, prompt(&agent), || agent.shutdown()).await
        })
        .map_err(|_| CliFailure::new("cannot drive the canary executor"))?
}

async fn prompt(agent: &Agent) -> CliResult<()> {
    let turn = agent
        .prompt("Reply with OK.")
        .await
        .map_err(|_| CliFailure::new("canary prompt was not admitted"))?;
    let outcome = turn
        .result()
        .await
        .map_err(|_| CliFailure::new("canary turn did not settle"))?;
    if !matches!(outcome.outcome(), TurnResultOutcome::Completed(_)) {
        return Err(CliFailure::new("canary turn did not complete"));
    }
    Ok(())
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
