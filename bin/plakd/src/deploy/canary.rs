use std::io::Write;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous::agent::{AgentBuilder, AgentConfig, AgentRequestPolicy, TurnResultOutcome};
use plakous::application::ApplicationExecutor;
use plakous::model::{
    CapabilityRequirement, GenerationLimits, ModelId, OutputBoundDisposition,
    ParallelToolDisposition, ProcessingTierDisposition, ReasoningEffort, ReasoningPolicy,
    SelectedProviderConfiguration, SessionLineage, ToolChoice,
};
use plakous::sessions::{
    CreateSessionOutcome, CreateSessionRequest, SessionOperationId, SessionRuntimeBinding,
};
use plakous::subscriptions::{NativeProvider, ResolvedProvider, SubscriptionRequestLimits};
use plakous::tools::{ToolRegistryBuilder, WorkspacePolicy};
use plakous_hosted::account::AccountRef;

use crate::commands::application::PlakousApplication;
use crate::product_config::ProductCompositionSelection;
use crate::shell::services::Services;
use crate::{CliFailure, CliResult, ProcessOutcome};

const DEADLINE: Duration = Duration::from_secs(90);
const OUTPUT_TOKENS: NonZeroU32 = NonZeroU32::new(16).expect("nonzero canary limit");

pub(crate) fn capabilities(stdout: &mut dyn Write) -> CliResult<ProcessOutcome> {
    writeln!(stdout, "plakd-deploy-canary-v1")
        .and_then(|()| {
            if cfg!(feature = "claude") {
                writeln!(stdout, "claude")?;
            }
            if cfg!(feature = "grok") {
                writeln!(stdout, "grok")?;
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
    let limits = SubscriptionRequestLimits::new(1, 4, 1, 16_384, Some(DEADLINE), 0)
        .map_err(|_| CliFailure::new("invalid canary request limits"))?;
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
    let records = services.session_records()?;
    let sessions = services.state_roots()?.runtime_sessions()?;
    let operation = SessionOperationId::new(record.create_operation_id.clone())
        .map_err(|_| CliFailure::new("invalid canary session identity"))?;
    // Publish only an archived record. Even a killed canary is never an active
    // product session that a serving host might dispatch or resume.
    records
        .save(&mut record)
        .map_err(|_| CliFailure::new("cannot archive the canary session"))?;
    executor
        .block_on(async {
            let create = sessions.create(CreateSessionRequest::new(
                operation,
                SessionRuntimeBinding::new(builder),
            ));
            let CreateSessionOutcome::Created { session, .. } =
                tokio::time::timeout_at(deadline.into(), create)
                    .await
                    .map_err(|_| CliFailure::new("canary session creation deadline expired"))?
                    .map_err(|_| CliFailure::new("cannot create the canary session"))?
            else {
                return Err(CliFailure::new("canary session was not newly created"));
            };
            record.plakous_locator = Some(session.locator());
            records
                .save(&mut record)
                .map_err(|_| CliFailure::new("cannot link the archived canary session"))?;
            let verdict = tokio::time::timeout_at(deadline.into(), async {
                let turn = session
                    .agent()
                    .prompt("Reply with OK.")
                    .await
                    .map_err(|_| CliFailure::new("canary prompt was not admitted"))?;
                let outcome = turn
                    .result()
                    .await
                    .map_err(|_| CliFailure::new("canary turn did not settle"))?;
                if matches!(outcome.outcome(), TurnResultOutcome::Completed(_)) {
                    Ok(())
                } else {
                    Err(CliFailure::new("canary turn did not complete"))
                }
            })
            .await;
            let locator = tokio::time::timeout(Duration::from_secs(10), async {
                let locator = session
                    .release()
                    .await
                    .map_err(|_| CliFailure::new("canary session did not release"))?;
                sessions
                    .archive(locator)
                    .await
                    .map_err(|_| CliFailure::new("canary runtime did not archive"))?;
                Ok::<_, CliFailure>(locator)
            })
            .await
            .map_err(|_| CliFailure::new("canary cleanup deadline expired"))??;
            record.plakous_locator = Some(locator);
            records
                .save(&mut record)
                .map_err(|_| CliFailure::new("cannot persist canary cleanup"))?;
            verdict.map_err(|_| CliFailure::new("canary turn deadline expired"))?
        })
        .map_err(|_| CliFailure::new("cannot drive the canary executor"))?
}
