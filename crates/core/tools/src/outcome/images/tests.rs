use std::str::FromStr;
use std::sync::{Arc, Mutex};

use super::*;
use crate::{
    ExecutionEnvironmentConfig, ExecutionEnvironmentScope, ImageNormalizer, PreparedImage,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Default)]
struct Normalizer {
    calls: Mutex<Vec<Vec<u8>>>,
    fail: bool,
}

impl ImageNormalizer for Normalizer {
    fn normalize(&self, bytes: &[u8]) -> Result<PreparedImage, String> {
        self.calls
            .lock()
            .map_err(|_| "recording lock")?
            .push(bytes.to_vec());
        if self.fail {
            return Err("invalid image data".to_owned());
        }
        Ok(PreparedImage::new("image/png", vec![9, 8, 7]))
    }
}

fn environment(
    accepts: bool,
    policy: bool,
    normalizer: Option<Arc<dyn ImageNormalizer>>,
) -> TestResult<(tempfile::TempDir, ExecutionEnvironment)> {
    let directory = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(plakous_model::RunId::from_str(
            "00000000-0000-0000-0000-000000000001",
        )?),
        directory.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let environment =
        crate::internal::install_visual_read(environment, "test", "test", accepts, normalizer);
    Ok((
        directory,
        crate::internal::install_tool_result_images(environment, policy),
    ))
}

fn outcome() -> TestResult<ToolOutcome> {
    Ok(ToolOutcome::success(vec![
        ToolResultPart::Text(TextBlock::new(
            r#"{"result":"metadata\n[image 1]\ncaption\n[image 2]"}"#,
            None,
        )?),
        ToolResultPart::Image(ImageBlock::new(
            "image/png",
            MediaSource::data_url("data:image/png;base64,AQID")?,
        )?),
        ToolResultPart::Image(ImageBlock::new(
            "image/png",
            MediaSource::bytes(vec![4, 5, 6])?,
        )?),
    ])?)
}

#[tokio::test]
async fn mcp_data_urls_and_generated_bytes_are_normalized_at_the_shared_boundary() -> TestResult {
    let normalizer = Arc::new(Normalizer::default());
    let (_directory, environment) = environment(true, true, Some(normalizer.clone()))?;
    let result = prepare_tool_images(outcome()?, &environment, &CancellationToken::new()).await?;
    assert_eq!(
        *normalizer.calls.lock().map_err(|_| "recording lock")?,
        vec![vec![1, 2, 3], vec![4, 5, 6]]
    );
    assert!(result.is_success());
    for part in &result.parts()[1..] {
        let ToolResultPart::Image(image) = part else {
            return Err("missing normalized image".into());
        };
        assert_eq!(image.source().as_bytes(), Some([9, 8, 7].as_slice()));
        assert_eq!(image.detail(), None);
    }
    Ok(())
}

#[tokio::test]
async fn prepared_images_skip_decode_but_not_route_admission() -> TestResult {
    let normalizer = Arc::new(Normalizer::default());
    let (_directory, environment) = environment(true, true, Some(normalizer.clone()))?;
    let result = prepare_tool_images(outcome()?, &environment, &CancellationToken::new()).await?;
    let repeated =
        prepare_tool_images(result.clone(), &environment, &CancellationToken::new()).await?;
    assert_eq!(result.parts(), repeated.parts());
    assert_eq!(
        normalizer.calls.lock().map_err(|_| "recording lock")?.len(),
        2
    );
    let environment = crate::internal::install_tool_result_images(environment, false);
    let omitted = prepare_tool_images(result, &environment, &CancellationToken::new()).await?;
    assert!(
        omitted
            .parts()
            .iter()
            .all(|part| !matches!(part, ToolResultPart::Image(_)))
    );
    Ok(())
}

struct BlockingNormalizer {
    started: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl ImageNormalizer for BlockingNormalizer {
    fn normalize(&self, bytes: &[u8]) -> Result<PreparedImage, String> {
        self.started.send(()).map_err(|_| "started channel")?;
        self.release
            .lock()
            .map_err(|_| "release lock")?
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "release timeout")?;
        Ok(PreparedImage::new("image/png", bytes.to_vec()))
    }
}

#[tokio::test]
async fn concurrent_decodes_hold_two_permits_until_blocking_work_finishes() -> TestResult {
    let (started, receiver) = std::sync::mpsc::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let normalizer: Arc<dyn ImageNormalizer> = Arc::new(BlockingNormalizer {
        started,
        release: Mutex::new(blocked),
    });
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let normalizer = normalizer.clone();
        jobs.push(tokio::spawn(async move {
            crate::internal::normalize_tool_image(
                normalizer,
                Ok(vec![1]),
                &CancellationToken::new(),
            )
            .await
        }));
    }
    let remaining = tokio::task::spawn_blocking(move || {
        receiver.recv_timeout(std::time::Duration::from_secs(5))?;
        receiver.recv_timeout(std::time::Duration::from_secs(5))?;
        Ok::<_, std::sync::mpsc::RecvTimeoutError>(receiver)
    })
    .await??;
    let available = crate::internal::IMAGE_DECODERS.available_permits();
    release.send(())?;
    release.send(())?;
    for job in jobs {
        job.await???;
    }
    assert_eq!(
        available, 0,
        "more than two concurrent decodes were admitted"
    );
    assert!(remaining.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn cancelled_decode_keeps_its_permit_until_the_worker_finishes() -> TestResult {
    let (started, receiver) = std::sync::mpsc::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let normalizer: Arc<dyn ImageNormalizer> = Arc::new(BlockingNormalizer {
        started,
        release: Mutex::new(blocked),
    });
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let task = tokio::spawn(async move {
        crate::internal::normalize_tool_image(normalizer, Ok(vec![1]), &token).await
    });
    tokio::task::spawn_blocking(move || receiver.recv_timeout(std::time::Duration::from_secs(5)))
        .await??;
    cancellation.cancel();
    let error = task
        .await?
        .err()
        .ok_or("cancelled decoder must stop waiting")?;
    let available = crate::internal::IMAGE_DECODERS.available_permits();
    release.send(())?;
    assert_eq!(error.kind(), crate::ToolRuntimeErrorKind::Cancelled);
    assert_eq!(
        available, 1,
        "a detached decoder lost its concurrency permit"
    );
    Ok(())
}

#[tokio::test]
async fn every_tool_source_omits_images_without_fact_policy_or_decoder() -> TestResult {
    for (accepts, policy, decoder) in [
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        let normalizer = Arc::new(Normalizer::default());
        let supplied: Option<Arc<dyn ImageNormalizer>> =
            decoder.then(|| normalizer.clone() as Arc<dyn ImageNormalizer>);
        let (_directory, environment) = environment(accepts, policy, supplied)?;
        let result =
            prepare_tool_images(outcome()?, &environment, &CancellationToken::new()).await?;
        assert!(result.is_success());
        let [ToolResultPart::Text(text)] = result.parts() else {
            return Err("images escaped gate".into());
        };
        let value: serde_json::Value = serde_json::from_str(text.text())?;
        assert_eq!(
            value["result"],
            "metadata\ncaption\n[2 image(s) omitted: this tool's result carries text only here]"
        );
        assert_eq!(
            *normalizer.calls.lock().map_err(|_| "recording lock")?,
            Vec::<Vec<u8>>::new()
        );
    }
    Ok(())
}

#[tokio::test]
async fn corrupt_tool_images_become_a_text_failure_with_metadata() -> TestResult {
    let normalizer = Arc::new(Normalizer {
        fail: true,
        ..Normalizer::default()
    });
    let (_directory, environment) = environment(true, true, Some(normalizer))?;
    let result = prepare_tool_images(outcome()?, &environment, &CancellationToken::new()).await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    let [ToolResultPart::Text(text)] = result.parts() else {
        return Err("failed image remained resident".into());
    };
    assert!(text.text().contains("metadata"));
    assert!(text.text().contains("invalid image data"));
    Ok(())
}
