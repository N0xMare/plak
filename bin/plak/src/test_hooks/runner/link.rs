//! Rendezvous outside RunnerClient locks, after the placement admission read.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous::runner::RunnerClient;
use plakous::tools::runner::{
    RunnerCall, RunnerCallFuture, RunnerFailure, RunnerLink, RunnerOpenFuture,
};

pub(crate) fn runner_link(client: RunnerClient) -> Arc<dyn RunnerLink> {
    match super::hold_cuts() {
        Some(directory) => Arc::new(CutLink { client, directory }),
        None => Arc::new(client),
    }
}

struct CutLink {
    client: RunnerClient,
    directory: PathBuf,
}

impl RunnerLink for CutLink {
    fn driver_operation_settled(
        &self,
        owner: plakous::tools::LineOwner,
        call_id: plakous::model::CallId,
        attempt_id: plakous::model::AttemptId,
    ) {
        self.client
            .driver_operation_settled(owner, call_id, attempt_id);
    }

    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        let client = self.client.clone();
        let directory = self.directory.clone();
        Box::pin(async move {
            let verb = call.verb().as_str();
            let payload_match = std::fs::read_to_string(directory.join("before-send.match")).ok();
            let before = std::fs::read_to_string(directory.join("before-send.verb"))
                .is_ok_and(|selected| selected.trim() == verb)
                && payload_match
                    .as_ref()
                    .is_none_or(|selected| call.payload().to_string().contains(selected.trim()));
            let lose_answer = std::fs::read_to_string(directory.join("lose-answer.verb"))
                .is_ok_and(|selected| selected.trim() == verb);
            if before && claim(&directory.join("before-send.entered")) {
                release(
                    &directory.join("before-send.release"),
                    Some(call.cancellation()),
                    None,
                )
                .await?;
                client.disconnect();
            }
            let answer = client.call(call).await;
            if lose_answer && answer.is_ok() && claim(&directory.join("lose-answer.entered")) {
                release(&directory.join("lose-answer.release"), None, None).await?;
                client.disconnect();
                tokio::time::sleep(super::runner_windows().effect).await;
                return Err(RunnerFailure::OutcomeUnknown(
                    "injected lost foreground answer".into(),
                ));
            }
            let marker = match &answer {
                Err(RunnerFailure::NotSent(_)) => Some("not-sent"),
                Err(RunnerFailure::ReadInterrupted(_)) => Some("read-interrupted"),
                _ => None,
            };
            if let Some(marker) = marker {
                let _ = std::fs::write(directory.join(marker), verb);
            }
            answer
        })
    }

    fn open(&self, call: RunnerCall) -> RunnerOpenFuture {
        self.client.open(call)
    }
}

pub(super) fn claim(path: &Path) -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .is_ok()
}

pub(super) async fn release(
    path: &Path,
    cancellation: Option<&plakous::model::CancellationToken>,
    client: Option<&RunnerClient>,
) -> Result<(), RunnerFailure> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while !path.is_file() {
        if client.is_some_and(|client| !client.is_connected()) {
            let _ = std::fs::write(path.with_extension("link-lost"), b"lost");
        }
        if cancellation.is_some_and(plakous::model::CancellationToken::is_cancelled) {
            return Err(RunnerFailure::NotSent("the cut's owner cancelled".into()));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RunnerFailure::Unavailable(
                "runner cut release timed out".into(),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    Ok(())
}
