//! The product image normalizer installed on a coding agent.
//!
//! Route, model and image-input acceptance are derived by the agent from the
//! bound provider configuration. This module only supplies the decoder. A
//! build without image decoding still installs it; `normalize_tool` refuses.

use std::sync::Arc;

use plakous::tools::{ImageNormalizer, PreparedImage};

use crate::attachments::{NormalizeError, normalize_tool};

const UNSUPPORTED_IMAGE: &str = "unable to process image: invalid or unsupported image data";
const IMAGE_READS_UNAVAILABLE: &str = "image reads are unavailable in this build";
const IMAGE_BUDGET: &str = "image does not fit the 1 MiB tool-result budget even downscaled";

/// The decoder for workspace image reads.
pub(crate) fn normalizer() -> Arc<dyn ImageNormalizer> {
    Arc::new(HostedImageNormalizer)
}

struct HostedImageNormalizer;

impl ImageNormalizer for HostedImageNormalizer {
    fn normalize(&self, bytes: &[u8]) -> Result<PreparedImage, String> {
        match normalize_tool(bytes) {
            Ok((media_type, bytes)) => Ok(PreparedImage::new(media_type, bytes)),
            Err(error) => Err(tool_normalize_error(error)),
        }
    }
}

fn tool_normalize_error(error: NormalizeError) -> String {
    match error {
        NormalizeError::Unfit => IMAGE_BUDGET.to_owned(),
        NormalizeError::Unavailable(_) => IMAGE_READS_UNAVAILABLE.to_owned(),
        NormalizeError::NotAnImage(_)
        | NormalizeError::Undecodable(_)
        | NormalizeError::Malformed { .. } => UNSUPPORTED_IMAGE.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_decoder_refuses_with_the_build_sentence() {
        let error = NormalizeError::Unavailable(crate::capabilities::CapabilityUnavailable {
            id: "images",
            kind: crate::capabilities::CapabilityKind::Surface,
            feature: "images",
        });
        assert_eq!(tool_normalize_error(error), IMAGE_READS_UNAVAILABLE);
    }

    #[cfg(not(feature = "images"))]
    #[test]
    fn normalize_tool_without_a_decoder_is_unavailable() {
        match normalize_tool(b"\x89PNG\r\n\x1a\n") {
            Err(error) => {
                assert!(matches!(error, NormalizeError::Unavailable(_)));
                assert_eq!(tool_normalize_error(error), IMAGE_READS_UNAVAILABLE);
            }
            Ok(_) => panic!("a build without image decoding decoded an image"),
        }
        match normalizer().normalize(b"\x89PNG\r\n\x1a\n") {
            Err(error) => assert_eq!(error, IMAGE_READS_UNAVAILABLE),
            Ok(_) => panic!("a build without image decoding returned image bytes"),
        }
    }
}

#[cfg(all(test, feature = "images"))]
mod executor {
    use std::fs;
    use std::io::Write as _;
    use std::str::FromStr;
    use std::sync::Arc;

    use plakous_compose::{CodingPresentationExt as _, standard_coding_packs_in_memory};
    use plakous_model::{
        AttemptId, CallId, CancellationToken, Clock, Digest, DigestDomain,
        EffectiveProviderConfigurationId, FixedClock, IntegrationClass, RunId, Timestamp,
        ToolCorrelation, ToolName, ToolResultPart, TurnId,
    };
    use plakous_tools::{
        CodingPresentation, EffectAuthorizationGrantSource, ExecutionEnvironment,
        ExecutionEnvironmentConfig, ExecutionEnvironmentScope, ImageNormalizer, JobConfig,
        ProgressSink, ProviderModelProjectionIdentityV2, ToolInvocation, ToolInvocationRequest,
        ToolOutcome, ToolOutcomeKind, ToolPolicyRevision, ToolProjectionMap, ToolRegistryBuilder,
        ToolRuntimeError, ToolRuntimeErrorKind, register_packs,
    };

    use super::normalizer;

    const UNSUPPORTED: &str = "unable to process image: invalid or unsupported image data";
    const UNAVAILABLE: &str = "image reads are unavailable in this build";
    const BINARY: &str = "file looks binary (it contains NUL bytes); this read returns text only";

    #[derive(Clone, Copy)]
    struct Bound {
        route: &'static str,
        model: &'static str,
        accepts: bool,
        decode: bool,
    }

    const CLAUDE: Bound = Bound {
        route: "anthropic.claude_subscription",
        model: "claude-test",
        accepts: true,
        decode: true,
    };
    const CODEX: Bound = Bound {
        route: "openai.codex",
        model: "gpt-5",
        accepts: true,
        decode: true,
    };

    fn png_bytes() -> Vec<u8> {
        encoded(4, 4, image::ImageFormat::Png)
    }

    fn encoded(width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
        use image::{ImageBuffer, Rgba};
        use std::io::Cursor;
        let image = ImageBuffer::from_fn(width, height, |x, y| {
            Rgba([
                u8::try_from(x).unwrap_or(1),
                u8::try_from(y).unwrap_or(1),
                128,
                255,
            ])
        });
        let dynamic = image::DynamicImage::ImageRgba8(image);
        let dynamic = if format == image::ImageFormat::Jpeg {
            image::DynamicImage::ImageRgb8(dynamic.to_rgb8())
        } else {
            dynamic
        };
        let mut bytes = Cursor::new(Vec::new());
        dynamic.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    fn gif_bytes() -> Vec<u8> {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame, ImageBuffer, Rgba};
        use std::io::Cursor;
        let frame = |seed: u8| ImageBuffer::from_fn(4, 4, |_, _| Rgba([seed, 20, 30, 255]));
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            encoder
                .encode_frame(Frame::from_parts(
                    frame(1),
                    0,
                    0,
                    Delay::from_numer_denom_ms(50, 1),
                ))
                .unwrap();
            encoder
                .encode_frame(Frame::from_parts(
                    frame(9),
                    0,
                    0,
                    Delay::from_numer_denom_ms(50, 1),
                ))
                .unwrap();
        }
        bytes.into_inner()
    }

    fn registry() -> plakous_tools::FrozenToolRegistry {
        let packs = standard_coding_packs_in_memory().unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register_packs(&packs, &mut builder).unwrap();
        builder.freeze()
    }

    fn projection(nested: bool) -> (plakous_tools::FrozenToolRegistry, ToolProjectionMap) {
        let frozen = registry();
        let effective: EffectiveProviderConfigurationId = serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(
                    DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[b"coding-presentation-result-contract-test"],
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let target = ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Standard,
            "anthropic.messages",
            "anthropic.claude_subscription",
            "anthropic.claude.compat.test",
            "messages-family",
            "claude-test",
            "claude-test",
            effective,
            1,
        )
        .unwrap();
        let map = if nested {
            CodingPresentation::Claude
                .compile_nested_projection(
                    JobConfig::default(),
                    frozen.clone(),
                    target,
                    &[ToolName::new("Read").unwrap()],
                )
                .unwrap()
        } else {
            CodingPresentation::Claude
                .projection_map(JobConfig::default(), frozen.clone(), target)
                .unwrap()
        };
        (frozen, map)
    }

    fn environment(root: &std::path::Path, bound: Bound) -> Arc<ExecutionEnvironment> {
        environment_with(root, bound, bound.decode.then(normalizer))
    }

    fn environment_with(
        root: &std::path::Path,
        bound: Bound,
        decoder: Option<Arc<dyn ImageNormalizer>>,
    ) -> Arc<ExecutionEnvironment> {
        Arc::new(plakous_tools::internal::install_visual_read(
            ExecutionEnvironment::host(
                ExecutionEnvironmentScope::Run(
                    RunId::from_str("00000000-0000-0000-0000-000000000061").unwrap(),
                ),
                root,
                ".",
                ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
            bound.route,
            bound.model,
            bound.accepts,
            decoder,
        ))
    }

    fn request(call: u8, tool: &str, arguments: &str) -> ToolInvocationRequest {
        ToolInvocationRequest::new(
            RunId::from_str("00000000-0000-0000-0000-000000000061").unwrap(),
            TurnId::from_str("00000000-0000-0000-0000-000000000062").unwrap(),
            AttemptId::from_str("00000000-0000-0000-0000-000000000063").unwrap(),
            CallId::from_str(&format!("00000000-0000-0000-0000-{call:012x}")).unwrap(),
            ToolName::new(tool).unwrap(),
            ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default()).unwrap(),
            Arc::<str>::from(arguments),
        )
    }

    async fn dispatch(
        frozen: &plakous_tools::FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        invocation: ToolInvocation,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        dispatch_with(frozen, environment, invocation, CancellationToken::new()).await
    }

    async fn dispatch_with(
        frozen: &plakous_tools::FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        invocation: ToolInvocation,
        cancellation: CancellationToken,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let revision = ToolPolicyRevision::new("portable-coding-test", 1).unwrap();
        let binding = plakous_tools::internal::authorization_binding(
            &invocation,
            environment.descriptor().clone(),
            revision,
        )?;
        let grant = plakous_tools::internal::authorization_grant(
            binding,
            EffectAuthorizationGrantSource::Policy,
        );
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(71, 0).unwrap()));
        let (progress, _receiver) =
            ProgressSink::channel(8, invocation.call_id(), Arc::clone(&clock))?;
        let context = plakous_tools::internal::tool_context(
            invocation.run_id(),
            invocation.turn_id(),
            invocation.attempt_id(),
            cancellation,
            None,
            clock,
            environment,
            grant,
            progress,
        );
        plakous_tools::internal::executor(frozen, &invocation)?
            .execute(invocation, context)
            .await
    }

    async fn view(
        root: &std::path::Path,
        path: &str,
        bound: Bound,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let frozen = registry();
        let environment = environment(root, bound);
        let invocation = frozen.prepare(request(
            0x10,
            "view_image",
            &serde_json::json!({"path": path}).to_string(),
        ))?;
        dispatch(&frozen, environment, invocation).await
    }

    async fn claude(
        root: &std::path::Path,
        path: &str,
        bound: Bound,
        nested: bool,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let (frozen, map) = projection(nested);
        let environment = environment(root, bound);
        let prepared = map.prepare(request(
            0x11,
            "Read",
            &serde_json::json!({"file_path": path}).to_string(),
        ))?;
        let (invocation, receipt) = prepared.into_parts();
        let outcome = dispatch(&frozen, environment, invocation).await?;
        map.raise(&receipt.expect("receipt"), outcome)
    }

    fn image_of(outcome: &ToolOutcome) -> (&str, &[u8]) {
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let [ToolResultPart::Image(image)] = outcome.parts() else {
            panic!("expected one image part");
        };
        (
            image.media_type(),
            image.source().as_bytes().expect("inline image"),
        )
    }

    fn failure(outcome: &ToolOutcome) -> &str {
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        let [ToolResultPart::Text(text)] = outcome.parts() else {
            panic!("expected failure text");
        };
        text.text()
    }

    fn kind(error: &ToolRuntimeError) -> ToolRuntimeErrorKind {
        error.kind()
    }

    #[tokio::test]
    async fn png_jpeg_and_webp_pass_through_and_gif_becomes_png() {
        let root = tempfile::tempdir().unwrap();
        let cases = [
            ("a.png", png_bytes(), "image/png", true),
            (
                "a.jpg",
                encoded(4, 4, image::ImageFormat::Jpeg),
                "image/jpeg",
                true,
            ),
            (
                "a.webp",
                encoded(4, 4, image::ImageFormat::WebP),
                "image/webp",
                true,
            ),
            ("a.gif", gif_bytes(), "image/png", false),
        ];
        for (name, bytes, media, identical) in cases {
            fs::write(root.path().join(name), &bytes).unwrap();
            for outcome in [
                view(root.path(), name, CODEX).await.unwrap(),
                claude(root.path(), name, CLAUDE, false).await.unwrap(),
            ] {
                let (got_media, got) = image_of(&outcome);
                assert_eq!(got_media, media, "{name}");
                if identical {
                    assert_eq!(got, bytes.as_slice(), "{name}");
                } else {
                    assert!(got.starts_with(b"\x89PNG"), "{name}");
                    assert_ne!(got, bytes.as_slice(), "{name}");
                }
            }
        }
    }

    #[tokio::test]
    async fn an_over_edge_image_is_downscaled() {
        let root = tempfile::tempdir().unwrap();
        let bytes = encoded(2050, 2, image::ImageFormat::Png);
        fs::write(root.path().join("wide.png"), &bytes).unwrap();
        for outcome in [
            view(root.path(), "wide.png", CODEX).await.unwrap(),
            claude(root.path(), "wide.png", CLAUDE, false)
                .await
                .unwrap(),
        ] {
            let (media, got) = image_of(&outcome);
            assert_eq!(media, "image/png");
            let (width, height) = plakous_tools::sniff_image(got)
                .unwrap()
                .dimensions()
                .unwrap();
            assert!(width <= 2048 && height <= 2048);
            assert_ne!(got, bytes.as_slice());
        }
    }

    #[tokio::test]
    async fn truncated_and_garbage_images_use_the_native_sentence() {
        let root = tempfile::tempdir().unwrap();
        let png = png_bytes();
        let mut truncated = png.clone();
        truncated.truncate(40);
        let mut garbage = png[..24].to_vec();
        garbage.extend_from_slice(&[7; 48]);
        fs::write(root.path().join("cut.png"), &truncated).unwrap();
        fs::write(root.path().join("bad.png"), &garbage).unwrap();
        for name in ["cut.png", "bad.png"] {
            let viewed = view(root.path(), name, CODEX).await.unwrap();
            assert_eq!(failure(&viewed), UNSUPPORTED, "{name}");
            let read = claude(root.path(), name, CLAUDE, false).await.unwrap();
            assert_eq!(failure(&read), UNSUPPORTED, "{name}");
        }
    }

    #[tokio::test]
    async fn a_model_without_image_input_is_named_in_the_tool_sentence() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.png"), png_bytes()).unwrap();
        let codex = Bound {
            accepts: false,
            ..CODEX
        };
        let viewed = view(root.path(), "a.png", codex).await.unwrap();
        assert_eq!(
            failure(&viewed),
            "view_image is not allowed because you do not support image inputs; route openai.codex model gpt-5"
        );
        let claude_bound = Bound {
            accepts: false,
            ..CLAUDE
        };
        let read = claude(root.path(), "a.png", claude_bound, false)
            .await
            .unwrap();
        assert_eq!(
            failure(&read),
            "route anthropic.claude_subscription model claude-test does not accept image input; Read cannot return images on this model"
        );
    }

    #[tokio::test]
    async fn a_build_without_a_normalizer_refuses_before_decoding() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.png"), png_bytes()).unwrap();
        let bound = Bound {
            decode: false,
            ..CODEX
        };
        assert_eq!(
            failure(&view(root.path(), "a.png", bound).await.unwrap()),
            UNAVAILABLE
        );
        let bound = Bound {
            decode: false,
            ..CLAUDE
        };
        assert_eq!(
            failure(&claude(root.path(), "a.png", bound, false).await.unwrap()),
            UNAVAILABLE
        );
    }

    /// Holds `normalize` until released, so a test can cancel mid-decode.
    struct SlowNormalizer {
        started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        finished: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl ImageNormalizer for SlowNormalizer {
        fn normalize(&self, bytes: &[u8]) -> Result<plakous_tools::PreparedImage, String> {
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
            let _ = self.release.lock().unwrap().recv();
            if let Some(finished) = self.finished.lock().unwrap().take() {
                let _ = finished.send(());
            }
            Ok(plakous_tools::PreparedImage::new(
                "image/png",
                bytes.to_vec(),
            ))
        }
    }

    #[tokio::test]
    async fn cancelling_during_normalization_returns_before_the_decode_finishes() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.png"), png_bytes()).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let slow: Arc<dyn ImageNormalizer> = Arc::new(SlowNormalizer {
            started: std::sync::Mutex::new(Some(started_tx)),
            release: std::sync::Mutex::new(release_rx),
            finished: std::sync::Mutex::new(Some(finished_tx)),
        });
        let environment = environment_with(root.path(), CODEX, Some(slow));
        let frozen = registry();
        let invocation = frozen
            .prepare(request(
                0x13,
                "view_image",
                &serde_json::json!({"path": "a.png"}).to_string(),
            ))
            .unwrap();
        let cancellation = CancellationToken::new();
        let call = tokio::spawn({
            let cancellation = cancellation.clone();
            async move { dispatch_with(&frozen, environment, invocation, cancellation).await }
        });
        started_rx.await.unwrap();
        cancellation.cancel();
        let returned = tokio::time::timeout(std::time::Duration::from_secs(10), call).await;
        // The decode is still blocked here; release it before asserting so a
        // failure does not leave the runtime waiting on the blocking pool.
        release_tx.send(()).unwrap();
        let Ok(joined) = returned else {
            panic!("cancellation waited for the decode");
        };
        let error = joined.unwrap().unwrap_err();
        assert_eq!(kind(&error), ToolRuntimeErrorKind::Cancelled, "{error}");
        finished_rx.await.unwrap();
    }

    #[tokio::test]
    async fn text_stays_text_and_a_canonical_read_does_not_return_an_image() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("notes.txt"), b"hello\n").unwrap();
        fs::write(root.path().join("a.png"), png_bytes()).unwrap();
        let read = claude(root.path(), "notes.txt", CLAUDE, false)
            .await
            .unwrap();
        assert_eq!(read.kind(), ToolOutcomeKind::Success);
        let [ToolResultPart::Text(text)] = read.parts() else {
            panic!("text read");
        };
        assert!(text.text().contains("hello"), "{}", text.text());
        let viewed = view(root.path(), "notes.txt", CODEX).await.unwrap();
        assert_eq!(failure(&viewed), UNSUPPORTED);

        let frozen = registry();
        let invocation = frozen
            .prepare(request(
                0x12,
                "read_file",
                &serde_json::json!({"path": "a.png"}).to_string(),
            ))
            .unwrap();
        let outcome = dispatch(&frozen, environment(root.path(), CLAUDE), invocation)
            .await
            .unwrap();
        assert_eq!(failure(&outcome), BINARY);
    }

    #[tokio::test]
    async fn symlink_and_parent_paths_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.png"), png_bytes()).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.png"),
            root.path().join("link.png"),
        )
        .unwrap();
        for path in ["link.png", "../secret.png"] {
            let viewed = view(root.path(), path, CODEX).await;
            let Err(error) = viewed else {
                panic!("{path} was viewed");
            };
            assert_eq!(
                kind(&error),
                ToolRuntimeErrorKind::WorkspacePolicy,
                "{path} {error}"
            );
            let read = claude(root.path(), path, CLAUDE, false).await;
            let Err(error) = read else {
                panic!("{path} was read");
            };
            assert_eq!(
                kind(&error),
                ToolRuntimeErrorKind::WorkspacePolicy,
                "{path} {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_image_over_the_workspace_file_cap_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("huge.png");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&png_bytes()).unwrap();
        file.set_len(plakous_tools::MAX_WORKSPACE_FILE_BYTES + 1)
            .unwrap();
        let error = view(root.path(), "huge.png", CODEX).await.unwrap_err();
        assert_eq!(kind(&error), ToolRuntimeErrorKind::ResourceLimit, "{error}");
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_job_output_path_stays_text_when_its_bytes_are_an_image() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let png = png_bytes();
        fs::write(workspace.path().join("shot.png"), &png).unwrap();
        let environment = environment(workspace.path(), CLAUDE);
        let sessions = environment.joined_process_sessions().unwrap();
        sessions.bind_job_outputs(
            plakous_tools::JobOutputRoot::open(&state.path().join("jobs")).unwrap(),
            plakous_model::SessionId::from_str("00000000-0000-0000-0000-0000000000b0").unwrap(),
            plakous_model::BranchId::from_str("00000000-0000-0000-0000-0000000000c0").unwrap(),
        );
        let command = format!(
            "cat {}; read line",
            workspace.path().join("shot.png").display()
        );
        let id = sessions
            .start(
                plakous_tools::JoinedProcessStartRequest::new(
                    plakous_tools::JoinedProcessCommand::shell(
                        plakous_tools::JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()])
                            .unwrap(),
                        &command,
                    )
                    .unwrap(),
                    ".",
                    Vec::new(),
                    true,
                    plakous_tools::JoinedProcessTerminalMode::Pipes,
                )
                .unwrap(),
                plakous_tools::OperationContext::new(CancellationToken::new()),
            )
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while sessions
            .poll_from(
                id,
                plakous_tools::JoinedProcessOutputCursor::new(0),
                4096,
                256,
            )
            .unwrap()
            .records()
            .iter()
            .map(|record| record.bytes().len())
            .sum::<usize>()
            < png.len()
        {
            assert!(std::time::Instant::now() < deadline, "no output");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        plakous_tools::internal::promote(&sessions, id).unwrap();
        let issued = sessions
            .job_output_file(plakous_tools::JoinedProcessSessions::job_number(id))
            .expect("issued")
            .path;
        let (frozen, map) = projection(false);
        let prepared = map
            .prepare(request(
                0x20,
                "Read",
                &serde_json::json!({"file_path": issued}).to_string(),
            ))
            .unwrap();
        let (invocation, receipt) = prepared.into_parts();
        let outcome = dispatch(&frozen, environment, invocation).await.unwrap();
        let raised = map.raise(&receipt.expect("receipt"), outcome).unwrap();
        assert_eq!(failure(&raised), BINARY);
    }
}
