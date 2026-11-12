//! Cancel abandoned construction probes and release their pending reply slots.

use super::{HostFrame, Inner, RunnerCall, RunnerClass, RunnerVerb, lock};

pub(super) struct PendingProbe<'a> {
    client: &'a Inner,
    request: &'a str,
}
impl<'a> PendingProbe<'a> {
    pub(super) fn new(client: &'a Inner, request: &'a str, call: &RunnerCall) -> Option<Self> {
        (call.verb() == RunnerVerb::ProbeWorkspace && matches!(call.class(), RunnerClass::Unkeyed))
            .then_some(Self { client, request })
    }
}
impl Drop for PendingProbe<'_> {
    fn drop(&mut self) {
        let abandoned = lock(&self.client.state)
            .pending
            .remove(self.request)
            .is_some();
        if abandoned {
            self.client.send(HostFrame::Cancel {
                request_id: self.request.to_owned(),
            });
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "isolated dropped-probe transport fixture"
)]
mod tests {
    use super::*;
    use crate::{ClientWindows, RunnerClient, RunnerIdentity, RunnerService};
    use plakous_runner_protocol::{Bind, RunnerFrame, VersionTriple, frame};
    use plakous_tools::internal::runner::RunnerLink as _;
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
    };
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn dropped_probe_removes_pending_reply_and_sends_cancel_without_affecting_other_calls() {
        let root = tempfile::tempdir().unwrap();
        let version = VersionTriple::current("fixture", "test");
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run("00000000-0000-4000-8000-000000000001".parse().unwrap()),
            root.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap();
        let service = RunnerService::new(
            environment,
            RunnerIdentity {
                runner_id: RunnerId::new("fixture").unwrap(),
                version: version.clone(),
                boot_id: "boot".into(),
                binary_id: None,
            },
        )
        .unwrap();
        let hello = service.hello().clone();
        let (host, mut peer) = tokio::io::duplex(65536);
        let (requested, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let (cancelled, mut cancellations) = tokio::sync::mpsc::unbounded_channel();
        let serving = tokio::spawn(async move {
            peer.write_all(&frame::encode(&RunnerFrame::Hello(hello)).unwrap())
                .await
                .unwrap();
            let mut frames = frame::FrameReader::new();
            let mut bytes = [0; 8192];
            loop {
                let count = peer.read(&mut bytes).await.unwrap();
                if count == 0 {
                    return;
                }
                frames.push(&bytes[..count]);
                while let Some(body) = frames.next_frame().unwrap() {
                    match frame::decode::<HostFrame>(&body).unwrap() {
                        HostFrame::Request(request) => requested.send(request.request_id).unwrap(),
                        HostFrame::Cancel { request_id } => cancelled.send(request_id).unwrap(),
                        _ => {}
                    }
                }
            }
        });
        let (client, _) = RunnerClient::connect(
            host,
            Bind {
                host_id: "fixture".into(),
                host_instance: "instance".into(),
                version,
                inspect: false,
            },
            ClientWindows::default(),
        )
        .await
        .unwrap();
        let call = RunnerCall::unkeyed(
            RunnerVerb::ProbeWorkspace,
            serde_json::json!({"op":"canonicalize", "path":"/workspace"}),
        );
        let pending = tokio::spawn(client.call(call));
        let id = requests.recv().await.unwrap();
        assert!(lock(&client.inner.state).pending.contains_key(&id));
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert!(!lock(&client.inner.state).pending.contains_key(&id));
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), cancellations.recv())
                .await
                .unwrap()
                .unwrap(),
            id
        );
        assert!(
            PendingProbe::new(
                &client.inner,
                "unused",
                &RunnerCall::unkeyed(RunnerVerb::ReadWorkspace, serde_json::Value::Null)
            )
            .is_none()
        );
        serving.abort();
    }
}
