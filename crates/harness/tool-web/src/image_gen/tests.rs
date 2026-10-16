#![allow(
    clippy::needless_pass_by_value,
    reason = "fixture builders take owned values for readability"
)]

use std::error::Error;
use std::str::FromStr as _;
use std::sync::Mutex;

use plakous_model::{
    AttemptId, CallId, Clock, Digest, DigestDomain, FixedClock, RunId, Timestamp, ToolCorrelation,
    TurnId,
};
use plakous_tools::internal::{
    authorization_binding, authorization_grant, bind_effect_dispatch, executor, tool_context,
};
use plakous_tools::{
    EffectAuthorizationGrantSource, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentScope, FrozenToolRegistry, ProgressSink, ToolInvocationRequest,
    ToolOutcomeKind, ToolOwnerClass, ToolPolicyRevision, ToolRegistryBuilder, ToolRegistryLimits,
    register_packs,
};

use super::*;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A 2x3 PNG header.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x03\x08\x06\0\0\0";

/// Answers every call with one fixed reply and records every request.
struct FakePort {
    reply: Result<Vec<u8>, RetrievalError>,
    max_edit_images: usize,
    takes_size: bool,
    requests: Mutex<Vec<ImageGenRequest>>,
}

impl FakePort {
    fn answering(reply: Result<Vec<u8>, RetrievalError>) -> Arc<Self> {
        Arc::new(Self {
            reply,
            max_edit_images: 5,
            takes_size: true,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<ImageGenRequest> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl ImageGenPort for FakePort {
    fn max_edit_images(&self) -> usize {
        self.max_edit_images
    }

    fn takes_size(&self) -> bool {
        self.takes_size
    }

    fn generate(
        &self,
        request: ImageGenRequest,
        _control: ExecutionOperationControl,
    ) -> ImageGenFuture {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request);
        }
        let reply = self.reply.clone();
        Box::pin(async move { reply })
    }
}

struct Workspace {
    dir: tempfile::TempDir,
    environment: Arc<ExecutionEnvironment>,
    registry: FrozenToolRegistry,
}

fn workspace(port: Arc<FakePort>) -> TestResult<Workspace> {
    let dir = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000901")?),
        dir.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits::default());
    register_packs(
        &[Arc::new(ImageGenPack::new(port)) as Arc<dyn ToolPack>],
        &mut builder,
    )?;
    Ok(Workspace {
        dir,
        environment,
        registry: builder.freeze(),
    })
}

async fn call(workspace: &Workspace, arguments: Value) -> TestResult<ToolOutcome> {
    let ids = (
        RunId::from_str("00000000-0000-0000-0000-000000000901")?,
        TurnId::from_str("00000000-0000-0000-0000-000000000902")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000903")?,
        CallId::from_str("00000000-0000-0000-0000-000000000904")?,
    );
    let invocation = workspace.registry.prepare(ToolInvocationRequest::new(
        ids.0,
        ids.1,
        ids.2,
        ids.3,
        ToolName::new(TOOL_NAME)?,
        ToolCorrelation::default(),
        arguments.to_string(),
    ))?;
    let intent = Digest::sha256(DigestDomain::Snapshot, 1, &[b"image-gen-test-intent"])?;
    let claim = Digest::sha256(DigestDomain::Snapshot, 1, &[b"image-gen-test-claim"])?;
    let invocation = bind_effect_dispatch(invocation, intent, claim);
    let binding = authorization_binding(
        &invocation,
        workspace.environment.descriptor().clone(),
        ToolPolicyRevision::new("image-gen-test", 1)?,
    )?;
    let grant = authorization_grant(binding, EffectAuthorizationGrantSource::Policy);
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(80, 0)?));
    let (progress, _receiver) = ProgressSink::channel(8, ids.3, Arc::clone(&clock))?;
    let context = tool_context(
        ids.0,
        ids.1,
        ids.2,
        plakous_model::CancellationToken::new(),
        None,
        clock,
        Arc::clone(&workspace.environment),
        grant,
        progress,
    );
    let executor = executor(&workspace.registry, &invocation)?;
    Ok(executor.execute(invocation, context).await?)
}

fn text_of(outcome: &ToolOutcome) -> String {
    outcome.joined_text()
}

fn images_in(outcome: &ToolOutcome) -> usize {
    outcome
        .parts()
        .iter()
        .filter(|part| matches!(part, ToolResultPart::Image(_)))
        .count()
}

#[test]
fn image_gen_is_a_serial_mutating_local_tool() -> TestResult {
    let pack = ImageGenPack::new(FakePort::answering(Ok(PNG.to_vec())));
    let [registration] = pack
        .registrations()?
        .try_into()
        .map_err(|_| "one registration")?;
    assert_eq!(registration.name().as_str(), "image_gen");
    assert_eq!(registration.owner(), &ToolOwnerClass::PlakLocal);
    assert!(registration.is_nested_eligible());
    let definition = registration.definition();
    assert_eq!(definition.effect(), ToolEffect::Mutating);
    assert_eq!(definition.concurrency(), ToolConcurrency::Serial);
    assert_eq!(definition.description(), PLAKOUS_WEB_IMAGE_GEN_DESCRIPTION);
    Ok(())
}

#[tokio::test]
async fn a_generated_image_is_written_and_shown() -> TestResult {
    let port = FakePort::answering(Ok(PNG.to_vec()));
    let workspace = workspace(Arc::clone(&port))?;
    std::fs::write(workspace.dir.path().join("in.png"), PNG)?;
    let outcome = call(
        &workspace,
        json!({"prompt": " a red fox ", "path": "out/fox.png", "size": "1024x1024",
               "edit_from": ["in.png"]}),
    )
    .await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert_eq!(
        text_of(&outcome),
        format!(
            "image_gen wrote out/fox.png: media_type=image/png dimensions=2x3 bytes={}",
            PNG.len()
        )
    );
    assert_eq!(images_in(&outcome), 1);
    assert_eq!(
        outcome.presentation_subject(),
        Some("out/fox.png (1024x1024)")
    );
    assert_eq!(
        std::fs::read(workspace.dir.path().join("out/fox.png"))?,
        PNG
    );
    assert_eq!(
        port.requests(),
        [ImageGenRequest {
            prompt: "a red fox".to_owned(),
            size: Some("1024x1024".to_owned()),
            edit_from: vec![ImageGenSource {
                media_type: "image/png",
                bytes: PNG.to_vec(),
            }],
        }]
    );
    Ok(())
}

#[tokio::test]
async fn a_large_image_is_written_but_not_shown() -> TestResult {
    let mut large = PNG.to_vec();
    large.resize(usize::try_from(MAX_STANDARD_IMAGE_BYTES)? + 1, 0);
    let port = FakePort::answering(Ok(large.clone()));
    let workspace = workspace(port)?;
    let outcome = call(&workspace, json!({"prompt": "fox", "path": "fox.png"})).await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert_eq!(images_in(&outcome), 0);
    assert!(text_of(&outcome).ends_with("is not shown; the file is in the workspace."));
    assert_eq!(std::fs::read(workspace.dir.path().join("fox.png"))?, large);
    Ok(())
}

#[tokio::test]
async fn an_existing_path_is_refused_before_the_request() -> TestResult {
    let port = FakePort::answering(Ok(PNG.to_vec()));
    let workspace = workspace(Arc::clone(&port))?;
    std::fs::write(workspace.dir.path().join("fox.png"), b"keep")?;
    let outcome = call(&workspace, json!({"prompt": "fox", "path": "fox.png"})).await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert!(text_of(&outcome).starts_with("image_gen refuses to write fox.png"));
    assert_eq!(
        std::fs::read(workspace.dir.path().join("fox.png"))?,
        b"keep"
    );
    assert_eq!(port.requests(), []);
    Ok(())
}

#[tokio::test]
async fn paths_outside_the_workspace_are_refused_before_the_request() -> TestResult {
    let port = FakePort::answering(Ok(PNG.to_vec()));
    let workspace = workspace(Arc::clone(&port))?;
    for arguments in [
        json!({"prompt": "fox", "path": "../fox.png"}),
        json!({"prompt": "fox", "path": "/etc/fox.png"}),
        json!({"prompt": "fox", "path": "fox.png", "edit_from": ["../in.png"]}),
    ] {
        assert!(
            call(&workspace, arguments.clone()).await.is_err(),
            "{arguments}"
        );
    }
    assert_eq!(port.requests(), []);
    Ok(())
}

#[tokio::test]
async fn arguments_the_route_cannot_serve_are_refused() -> TestResult {
    let port = Arc::new(FakePort {
        reply: Ok(PNG.to_vec()),
        max_edit_images: 1,
        takes_size: false,
        requests: Mutex::new(Vec::new()),
    });
    let workspace = workspace(Arc::clone(&port))?;
    for (arguments, message) in [
        (
            json!({"prompt": "fox", "path": "a.png", "size": "1024x1024"}),
            "size is not available on this route; omit it",
        ),
        (
            json!({"prompt": "fox", "path": "a.png", "edit_from": ["x.png", "y.png"]}),
            "edit_from names 2 images; this route edits at most 1",
        ),
    ] {
        let error = call(&workspace, arguments).await.err().ok_or("accepted")?;
        assert!(error.to_string().contains(message), "{error}");
    }
    let outcome = call(
        &workspace,
        json!({"prompt": "fox", "path": "a.png", "edit_from": ["missing.png"]}),
    )
    .await?;
    assert_eq!(
        text_of(&outcome),
        "image_gen cannot edit missing.png: the file does not exist"
    );
    assert_eq!(port.requests(), []);
    Ok(())
}

#[test]
fn sizes_are_width_by_height_or_auto() {
    for size in ["auto", "1024x1024", "1536x1024", "64x64"] {
        assert!(valid_size(size), "{size}");
    }
    for size in [
        "",
        "1024",
        "0x1",
        "01024x1024",
        "1024X1024",
        "x1024",
        "123456x1",
    ] {
        assert!(!valid_size(size), "{size}");
    }
}

#[tokio::test]
async fn provider_failures_and_non_images_write_nothing() -> TestResult {
    for (reply, message) in [
        (
            Err(RetrievalError::new(
                RetrievalErrorKind::Rejected,
                "the provider refused the side request (HTTP 400)",
            )),
            "image_gen failed: the provider refused the side request (HTTP 400)",
        ),
        (
            Ok(b"<html>".to_vec()),
            "image_gen failed: the provider returned bytes that are not a supported image",
        ),
    ] {
        let workspace = workspace(FakePort::answering(reply))?;
        let outcome = call(&workspace, json!({"prompt": "fox", "path": "fox.png"})).await?;
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(text_of(&outcome), message);
        assert!(!workspace.dir.path().join("fox.png").exists());
    }
    Ok(())
}
