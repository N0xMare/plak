//! A built-in provider on the Direct path, with the API key read from a file.
//!
//! Run it with:
//!
//! ```sh
//! cargo run -p plakous --example provider_direct --features openrouter -- \
//!     /path/to/openrouter-key mistralai/mistral-small-3.2-24b-instruct:free
//! ```
//!
//! The first argument is a **path**, never the key text: a key on a command
//! line leaks into the process table and the shell history. Nothing here reads
//! the environment for a credential, and nothing falls back to another route if
//! this one refuses. With no arguments the example constructs nothing and
//! explains what it needs, which is also how the offline gate runs it.
//!
//! Discovery and the turn are the only network operations, and both are billed
//! by the provider.

use std::sync::Arc;

use plakous::agent::{AgentBuilder, TurnResultOutcome};
use plakous::model::{
    CachePolicy, ContextAdmissionRequirement, LocalDurabilityDisposition, ModelAdapterProfileId,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, ProcessingTierDisposition,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
    ReplayDisposition, ResponseStateDisposition, RetentionDisposition, ToolChoice,
    ToolPresentationMode,
};
use plakous::openrouter::{OpenRouterApiKey, OpenRouterChatClient, StaticOpenRouterApiKeySource};
use plakous::tools::{ToolRegistryBuilder, WorkspacePolicy};

/// The error type this example returns.
type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// Reads one API key from a file, trimming only the trailing newline.
fn read_key(path: &str) -> Fallible<OpenRouterApiKey> {
    let text = std::fs::read_to_string(path)?;
    Ok(OpenRouterApiKey::new(text.trim_end_matches(['\n', '\r']))?)
}

#[tokio::main]
async fn main() -> Fallible<()> {
    let mut arguments = std::env::args().skip(1);
    let (Some(key_path), Some(model)) = (arguments.next(), arguments.next()) else {
        println!(
            "provider_direct needs a key-file path and one exact model literal, \
             for example: provider_direct ./openrouter-key openai/gpt-4o-mini"
        );
        return Ok(());
    };

    // The credential is caller-owned and explicit. `account` and `authority`
    // are the caller's own non-secret labels for this key.
    let source = Arc::new(StaticOpenRouterApiKeySource::new(
        read_key(&key_path)?,
        "example-account",
        "example-key-authority",
    )?);
    let client = OpenRouterChatClient::connect(source)?;

    // Discovery authenticates one exact literal. An unknown or ambiguous model
    // is refused; there is no catalog guess and no nearest match.
    let evidence = client.discover_exact(&model).await?;
    // The input envelope is caller policy, bounded by authenticated discovery.
    let context = client.custom_context_contract(
        evidence.context_length(),
        evidence.context_length().min(24_576),
    )?;
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::None,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::NoRequestBound,
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            // This identifies our retention contract, not the model adapter.
            profile: ModelAdapterProfileId::new("example-retention-contract")?,
            revision: ProviderProfileRevisionId::new("example-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    };
    let configured = client.configured_gateway_managed_behavior(evidence, context, behavior)?;

    let directory = tempfile::tempdir()?;
    // Stores and workspaces refuse symlinked ancestors, and macOS temp paths
    // start at the /var and /tmp symlinks.
    let root = std::fs::canonicalize(directory.path())?;
    let agent = AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(&root, ".")?),
    )
    .build()?;
    let result = agent
        .prompt("Reply with the word ready.")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    agent.shutdown().await?;
    println!(
        "provider Direct: discovered {model} from an explicit key file and completed one turn"
    );
    Ok(())
}
