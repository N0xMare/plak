//! Model tools over the real framed runner link, including repeated reads.

#![allow(clippy::unwrap_used, reason = "integration test fixtures")]

use std::str::FromStr;
use std::sync::Arc;

use plakous_agent::internal::{EffectAuthorization, ToolExecutionControl, ToolRuntime};
use plakous_model::{
    AttemptId, BranchId, CallId, CancellationToken, Clock, Digest, DigestDomain, LineId, RunId,
    SystemClock, ToolCorrelation, ToolName, ToolResultPart, TurnId,
};
use plakous_runner::{ClientWindows, Loopback, RunnerIdentity, RunnerService};
use plakous_runner_protocol::{Bind, VersionTriple};
use plakous_tools::internal::runner::runner_environment;
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, ImageNormalizer,
    LineOwner, PreparedImage, ProgressSink, ToolExposure, ToolExposureMap, ToolInvocationRequest,
    ToolOutcome, ToolOutcomeKind, ToolPack, ToolPolicy, ToolPolicyDecision, ToolPolicyFuture,
    ToolPolicyRevision, ToolRegistryBuilder,
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Allow;

impl ToolPolicy for Allow {
    fn revision(&self) -> ToolPolicyRevision {
        ToolPolicyRevision::new("test.runner-reads", 1).unwrap()
    }

    fn evaluate(&self, _: plakous_tools::ToolPolicyRequest) -> ToolPolicyFuture {
        Box::pin(async { Ok(ToolPolicyDecision::Allowed) })
    }
}

// Keep normalization independent of decoding: the test checks the exact bytes
// the sliced runner read delivers to the injected normalizer.
struct ExactImage(Vec<u8>);

impl ImageNormalizer for ExactImage {
    fn normalize(&self, bytes: &[u8]) -> Result<PreparedImage, String> {
        assert_eq!(bytes, self.0);
        Ok(PreparedImage::new("image/png", bytes.to_vec()))
    }
}

struct Placed {
    _link: Loopback,
    runtime: ToolRuntime,
    exposed: ToolExposureMap,
    run: RunId,
}

impl Placed {
    async fn new(root: &std::path::Path, image: Vec<u8>) -> TestResult<Self> {
        let run = RunId::from_str("00000000-0000-0000-0000-000000000041")?;
        let scope = ExecutionEnvironmentScope::Run(run);
        let local =
            ExecutionEnvironment::host(scope, root, ".", ExecutionEnvironmentConfig::default())?;
        let version = VersionTriple::current("runner-reads", "debug");
        let service = RunnerService::new(
            local,
            RunnerIdentity {
                runner_id: plakous_tools::RunnerId::new("loopback")?,
                version: version.clone(),
                boot_id: "boot".into(),
                binary_id: None,
            },
        )?;
        let link = Loopback::join(
            service,
            Bind {
                host_id: "host".into(),
                host_instance: "instance".into(),
                version,
                inspect: false,
            },
            ClientWindows::default(),
        )
        .await?;
        let environment = runner_environment(
            scope,
            root,
            ".",
            ExecutionEnvironmentConfig::default(),
            link.backend(),
        )?
        .with_line_owner(LineOwner::new(
            LineId::from_str("00000000-0000-0000-0000-0000000000a1")?,
            1,
            BranchId::from_str("00000000-0000-0000-0000-0000000000b1")?,
            1,
        ));
        let environment = plakous_tools::internal::install_visual_read(
            environment,
            "test",
            "image-model",
            true,
            Some(Arc::new(ExactImage(image))),
        );
        let mut builder = ToolRegistryBuilder::default();
        for pack in [
            Arc::new(plakous_tool_files::FilesToolPack::default()) as Arc<dyn ToolPack>,
            Arc::new(plakous_tool_search::SearchToolPack),
        ] {
            for registration in pack.registrations()? {
                builder.register(registration)?;
            }
        }
        let registry = builder.freeze();
        let read = registry
            .identity(&ToolName::new("read_file")?)
            .ok_or("missing read")?
            .clone();
        let exposed = ToolExposureMap::new(registry.clone(), [ToolExposure::new(read, "Read")?])?;
        Ok(Self {
            _link: link,
            runtime: ToolRuntime::new(registry, Arc::new(environment), Arc::new(Allow)),
            exposed,
            run,
        })
    }

    async fn invoke(
        &self,
        name: &str,
        arguments: Value,
        call: u8,
        effect: bool,
    ) -> TestResult<ToolOutcome> {
        let call = CallId::from_str(&format!("00000000-0000-0000-0000-{call:012x}"))?;
        let request = ToolInvocationRequest::new(
            self.run,
            TurnId::from_str("00000000-0000-0000-0000-000000000042")?,
            AttemptId::from_str("00000000-0000-0000-0000-000000000043")?,
            call,
            ToolName::new(name)?,
            ToolCorrelation::default(),
            Arc::<str>::from(arguments.to_string()),
        );
        let invocation = if name == "Read" {
            self.exposed.prepare(request)?
        } else {
            self.runtime.prepare(request)?
        };
        let EffectAuthorization::Allowed(authorized) = self.runtime.authorize(invocation).await?
        else {
            return Err("not authorized".into());
        };
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let (progress, _receiver) = ProgressSink::channel(32, call, Arc::clone(&clock))?;
        let control = ToolExecutionControl::new(CancellationToken::new(), None, clock, progress);
        let outcome = if effect {
            let intent = self.runtime.effect_dispatch_intent(&authorized)?;
            let claimed = self.runtime.claim_effect(
                authorized,
                intent,
                Digest::sha256(DigestDomain::Snapshot, 1, &[b"durable-test-claim"])?,
            )?;
            Box::pin(self.runtime.execute_claimed(claimed, control))
                .await
                .into_result()
                .map_err(|error| error.message().as_str().to_owned())?
        } else {
            self.runtime
                .execute(authorized, control)
                .await
                .map_err(|error| error.message().as_str().to_owned())?
        };
        assert_eq!(
            outcome.kind(),
            ToolOutcomeKind::Success,
            "{:?}",
            outcome.parts()
        );
        Ok(outcome)
    }
}

fn text(outcome: &ToolOutcome) -> String {
    outcome
        .parts()
        .iter()
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_reads_claude_small_text() -> TestResult {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("small.txt"), "alpha\nbeta\n")?;
    let placed = Placed::new(root.path(), vec![]).await?;
    let outcome = placed
        .invoke("Read", json!({"path":"small.txt"}), 1, false)
        .await?;
    assert!(text(&outcome).contains("beta"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_reads_claude_image_resumes() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut image = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01".to_vec();
    image.resize(64 * 1024, 0);
    std::fs::write(root.path().join("image.png"), &image)?;
    let placed = Placed::new(root.path(), image).await?;
    let outcome = placed
        .invoke("Read", json!({"path":"image.png"}), 1, false)
        .await?;
    assert!(matches!(
        outcome.parts().first(),
        Some(ToolResultPart::Image(_))
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_reads_text_multiple_slices() -> TestResult {
    let root = tempfile::tempdir()?;
    let content = format!(
        "{}last-line\n",
        format!("{}\n", "x".repeat(1000)).repeat(1700)
    );
    std::fs::write(root.path().join("large.txt"), content)?;
    let placed = Placed::new(root.path(), vec![]).await?;
    let outcome = placed
        .invoke(
            "read_file",
            json!({"path":"large.txt", "offset":1701,"limit":1}),
            1,
            false,
        )
        .await?;
    assert!(text(&outcome).contains("last-line"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_reads_patch_two_existing_files() -> TestResult {
    let root = tempfile::tempdir()?;
    for path in ["a.txt", "b.txt"] {
        std::fs::write(root.path().join(path), "before\n")?;
    }
    let placed = Placed::new(root.path(), vec![]).await?;
    let patch = ["a.txt", "b.txt"].map(|path| format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-before\n+after\n")).join("");
    placed
        .invoke("apply_patch", json!({"patch":patch}), 1, true)
        .await?;
    for path in ["a.txt", "b.txt"] {
        assert_eq!(std::fs::read_to_string(root.path().join(path))?, "after\n");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_reads_search_context_multiple_files() -> TestResult {
    let root = tempfile::tempdir()?;
    for path in ["a.txt", "b.txt"] {
        std::fs::write(root.path().join(path), "before\nneedle\nafter\n")?;
    }
    let placed = Placed::new(root.path(), vec![]).await?;
    let outcome = placed
        .invoke(
            "search_repository",
            json!({"path":".","query":"needle", "use_regex":true, "context":1}),
            1,
            false,
        )
        .await?;
    let text = text(&outcome);
    for expected in ["a.txt", "b.txt", "before", "after"] {
        assert!(text.contains(expected), "{text}");
    }
    Ok(())
}
