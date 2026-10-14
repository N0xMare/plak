//! Consumer-owned JavaScript composition in the `plakous-js-host` worker.
//!
//! Build the worker once, then run the example against it:
//!
//! ```sh
//! cargo build -p plakous-js-runtime --bin plakous-js-host
//! cargo run -p plakous --example worker_js_sidecar \
//!     --features composition,sessions-sqlite -- target/debug/plakous-js-host
//! ```
//!
//! A Custom model uses the neutral Pi vocabulary, not Codex enrollment. The
//! `composition` feature adds the presentation and sidecar client, not an
//! in-process evaluator. The example canonicalizes the explicit worker path;
//! attachment records its path and SHA-256, not proof of its provenance. The
//! library never searches for a worker or falls back to another interpreter.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous::agent::{
    AgentBuilder, HarnessProfileId, LocalCompositionAttachment, LocalCompositionEngineArtifact,
    LocalCompositionProfile, ProductExtensionEvidence, ProductExtensionLimits, TurnResultOutcome,
    attach_local_composition,
};
use plakous::construction::{compile_local_composition_plan, provider_model_projection_identity};
use plakous::model::{
    ConfiguredLanguageModel, ContentBlock, Digest, DigestDomain, LanguageModel,
    LocalDurabilityDisposition, ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract,
    ToolName, ToolOutputContract, ToolPresentationMode, ToolResultPart,
};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
    SessionRuntimeBinding,
};
use plakous::tools::{
    CodingPresentation, ProductJavaScriptCompositionV1, RegisteredToolId, ToolContext,
    ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation,
    ToolOutcome, ToolRegistration, ToolRegistryBuilder, WorkspacePolicy, register_packs,
    standard_coding_packs_in_memory,
};

#[path = "common/scripted.rs"]
mod scripted;

use scripted::{Fallible, ScriptedModel, Turn, Vocabulary};

/// The embedder's own nested tool. JavaScript reaches it as `tools.answer`.
#[derive(Default)]
struct Answer {
    calls: AtomicUsize,
}

impl ToolExecutor for Answer {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolOutcome::success_text("41") })
    }
}

#[tokio::main]
#[allow(
    clippy::too_many_lines,
    reason = "one example shows one complete workflow in a single readable flow"
)]
async fn main() -> Fallible<()> {
    let host = std::env::args()
        .nth(1)
        .ok_or("pass the plakous-js-host path as the first argument")?;
    let host = std::fs::canonicalize(host)?;
    let directory = tempfile::tempdir()?;
    // Stores and workspaces refuse symlinked ancestors, and macOS temp paths
    // start at the /var and /tmp symlinks.
    let root = std::fs::canonicalize(directory.path())?;
    let model = Arc::new(ScriptedModel::new(
        &Vocabulary::direct(),
        vec![Turn::Call {
            name: "exec_js",
            input: serde_json::json!({"source": "text(Number(await tools.answer({unused: true})) + 1);"}).to_string(),
        }],
    )?);
    let answer = Arc::new(Answer::default());
    let nested = ToolName::new("answer")?;

    let mut registry = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut registry)?;
    ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
    registry.register(
        ToolRegistration::new(
            RegisteredToolId::new("example.answer")?,
            ToolExecutorId::new("example.answer.v1")?,
            ToolExecutorBindingIdentity::new(1, &[b"example.answer.v1"])?,
            ToolDefinition::new(
                nested.clone(),
                "1.0.0",
                "Return the embedder's numeric answer.",
                ToolInputContract::structured(serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"unused": {"type": "boolean"}},
                    "required": ["unused"],
                    "additionalProperties": false
                }))?,
                ToolOutputContract::utf8(256)?,
                ToolConcurrency::Serial,
                ToolEffect::ReadOnly,
            )?,
            answer.clone(),
        )
        .nested_eligible(),
    )?;
    let registry = registry.freeze();

    let outer = registry
        .definitions()
        .iter()
        .filter(|definition| matches!(definition.name().as_str(), "exec_js" | "wait_js"))
        .cloned()
        .collect::<Vec<_>>();
    let configuration = model.provider_configuration(
        ToolPresentationMode::Direct,
        LocalDurabilityDisposition::DurableRequired,
        &outer,
    )?;
    let target = provider_model_projection_identity(&configuration)?;
    let composition = ProductJavaScriptCompositionV1::compile_for_kind(
        plakous::tools::JobConfig::default(),
        registry.clone(),
        target,
        CodingPresentation::Pi,
        [nested],
    )?;
    let profile = LocalCompositionProfile::product_javascript_v1(&composition)?;
    let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(host)?;
    let issuer = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"example-embedder"])?;
    let qualification = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"example-offline-candidate"],
    )?;
    let evidence = ProductExtensionEvidence::product_javascript_v1(
        &profile,
        &artifact,
        ProductExtensionLimits {
            max_source_bytes: 1_048_576,
            max_yield_time_ms: 10_000,
            max_output_tokens: 10_000,
        },
        issuer,
        qualification,
        &configuration,
    )?;
    let attachment = LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .coding_presentation_kind(CodingPresentation::Pi)
        .product_extension_evidence(evidence)
        .build()?;
    let workspace = Arc::new(WorkspacePolicy::host(&root, ".")?);
    let language_model: Arc<dyn LanguageModel> = model.clone();
    let plan = compile_local_composition_plan(
        HarnessProfileId::new("example.sidecar")?,
        &language_model,
        &configuration,
        &registry,
        &workspace,
    )?;
    let configured =
        ConfiguredLanguageModel::custom(model.clone(), configuration, model.authority.clone())?;
    let builder = AgentBuilder::new(configured, registry, workspace).harness_plan(plan);

    let sessions = AgentSessions::new(SessionBackend::sqlite(root.join("sidecar.sqlite3"))?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("example-sidecar")?,
            SessionRuntimeBinding::new(attach_local_composition(builder, Some(attachment))?),
        ))
        .await?
    else {
        return Err("create did not produce a new session".into());
    };
    let agent = session.agent();
    let result = agent
        .prompt("evaluate the nested tool's answer")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let outputs = result
        .conversation()
        .items()
        .iter()
        .flat_map(plakous::model::ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(tool_result) => Some(tool_result.parts()),
            _ => None,
        })
        .flatten()
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        outputs
            .iter()
            .any(|output| output.lines().any(|line| line == "42")),
        "the worker must report the nested tool's 41 plus one: {outputs:?}"
    );
    assert_eq!(answer.calls.load(Ordering::SeqCst), 1);
    agent.shutdown().await?;
    session.release().await?;
    println!("worker JS sidecar: the script called the embedder's nested tool and returned 42");
    Ok(())
}
