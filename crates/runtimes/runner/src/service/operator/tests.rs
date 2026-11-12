use super::super::*;
use crate::operator::tests::congested_link_fixture;
use plakous_runner_protocol::operator::{OperatorAnswer, OperatorOperation};
use plakous_tools::{ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId};
use tokio::io::AsyncWriteExt as _;

fn runner(root: &Path, operator: crate::operator::OperatorTerminalService) -> RunnerService {
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
        root,
        root,
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let mut service = RunnerService::new(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("fixture").unwrap(),
            version: VersionTriple::current("fixture", "debug"),
            boot_id: "boot".into(),
            binary_id: None,
        },
    )
    .unwrap();
    let inner = Arc::get_mut(&mut service.inner).unwrap();
    inner.operator = Some(operator);
    inner.dead = Duration::from_secs(2);
    service
}

#[tokio::test]
async fn operator_stream_full_bulk_channel_gates_input_and_preserves_ack_sequence() {
    let (operator, stream, frames) = congested_link_fixture();
    let root = tempfile::tempdir().unwrap();
    let service = runner(root.path(), operator.clone());
    let (bulk, mut receiving) = mpsc::channel(4);
    for frame in frames {
        bulk.try_send(RunnerFrame::Stream(frame)).unwrap();
    }
    let (mut client, incoming) = tokio::io::duplex(8192);
    let (control, _) = mpsc::unbounded_channel();
    let closer = CancellationToken::new();
    let cancel = closer.clone();
    let run = service.clone();
    let task = tokio::spawn(async move {
        run.serve_bound_connection(
            "paired",
            "generation",
            &mut FrameStream::new(incoming),
            control,
            bulk,
            cancel,
        )
        .await;
    });
    let payload = serde_json::json!({"terminal_event":"data","offset":0,"bytes_b64":"YQ=="});
    client
        .write_all(
            &plakous_runner_protocol::frame::encode(&HostFrame::Stream(StreamFrame {
                stream,
                seq: 1,
                event: plakous_runner_protocol::StreamEvent::Data { payload },
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    for seq in 0..2 {
        let RunnerFrame::Stream(frame) = receiving.recv().await.unwrap() else {
            panic!("bulk")
        };
        assert_eq!(frame.seq, seq);
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let namespace = plakous_runner_protocol::operator::OperatorNamespace {
        state_key: "state".to_owned().try_into().unwrap(),
        principal: "operator".to_owned().try_into().unwrap(),
    };
    let OperatorAnswer::Table { terminals } = operator.execute(
        "paired",
        "generation",
        0,
        namespace,
        "list",
        OperatorOperation::List,
    ) else {
        panic!("table")
    };
    assert_eq!(
        terminals[0].input_next, 0,
        "two free slots are below the three-slot reserve"
    );
    for seq in 2..=4 {
        let RunnerFrame::Stream(frame) =
            tokio::time::timeout(Duration::from_secs(1), receiving.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("stream")
        };
        assert_eq!(frame.seq, seq);
        if seq == 4 {
            assert!(
                matches!(frame.event, plakous_runner_protocol::StreamEvent::Data { payload }
                if payload["terminal_event"] == "input_accepted" && payload["next"] == 1)
            );
        }
    }
    closer.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn operator_stream_congested_read_gate_honors_read_deadline() {
    let (operator, _, frames) = congested_link_fixture();
    let root = tempfile::tempdir().unwrap();
    let mut service = runner(root.path(), operator);
    Arc::get_mut(&mut service.inner).unwrap().dead = Duration::from_millis(150);
    let (bulk, _receiving) = mpsc::channel(4);
    for frame in frames {
        bulk.try_send(RunnerFrame::Stream(frame)).unwrap();
    }
    let (_client, incoming) = tokio::io::duplex(8192);
    let (control, _) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(1),
        service.serve_bound_connection(
            "paired",
            "generation",
            &mut FrameStream::new(incoming),
            control,
            bulk,
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap();
}
