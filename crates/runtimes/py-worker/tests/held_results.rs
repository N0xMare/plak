//! Raw worker frames so the receipt's order relative to `CellDone` is observable.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "real-worker protocol assertions"
)]

use std::{
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::Duration,
};

use plakous_cell::{
    BindingInput, CELL_PROTOCOL_REVISION, CellOutcome, CellPrelude, CellValue, DeliveryVerdict,
    EngineConfig, HostFrame, MAX_FRAME_BYTES, MontyBinding, MontyBootstrap, WorkerFrame,
    read_frame_blocking, write_frame_blocking,
};

struct Worker {
    child: Child,
    input: ChildStdin,
    output: Receiver<WorkerFrame>,
}

impl Worker {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_plakous-py-host"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("worker");
        let input = child.stdin.take().expect("stdin");
        let mut stdout = child.stdout.take().expect("stdout");
        let (tx, output) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(bytes)) = read_frame_blocking(&mut stdout, MAX_FRAME_BYTES) {
                if tx
                    .send(serde_json::from_slice(&bytes).expect("worker frame"))
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut worker = Self {
            child,
            input,
            output,
        };
        assert!(matches!(worker.read(), WorkerFrame::Hello { .. }));
        worker.send(&HostFrame::Configure {
            revision: CELL_PROTOCOL_REVISION.into(),
            engine: EngineConfig::MONTY,
        });
        worker
    }

    fn send(&mut self, frame: &HostFrame) {
        write_frame_blocking(&mut self.input, frame).expect("parent frame");
    }

    fn read(&self) -> WorkerFrame {
        self.output
            .recv_timeout(Duration::from_secs(10))
            .expect("worker advances without another frame")
    }

    fn call(&self, expected: &str) -> u64 {
        let frame = self.read();
        let WorkerFrame::NestedCall { call, name, .. } = frame else {
            panic!("expected a nested call: {frame:?}");
        };
        assert_eq!(name, expected);
        call
    }

    fn reply(&mut self, call: u64, value: i64) {
        self.send(&HostFrame::NestedResult {
            cell: 1,
            call,
            outcome: Ok(CellValue::Int(value)),
        });
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn an_early_result_is_resumed_on_later_await_or_declined_before_the_terminal() {
    for (suffix, value) in [("await first", "42"), ("first.cancel()\n7", "7")] {
        let mut worker = Worker::spawn();
        worker.send(&HostFrame::StartCell {
            cell: 1,
            source: format!("first = _early()\nawait _gate()\n{suffix}"),
            prelude: CellPrelude::Monty {
                bootstrap: Some(MontyBootstrap {
                    source: String::new(),
                    bindings: [("_early", "early"), ("_gate", "gate")]
                        .into_iter()
                        .map(|(function, tool)| MontyBinding {
                            function: function.into(),
                            tool: tool.into(),
                            input: BindingInput::Dictionary,
                        })
                        .collect(),
                }),
            },
            window: None,
        });
        let early = worker.call("early");
        let gate = worker.call("gate");
        assert!(matches!(worker.read(), WorkerFrame::Waiting { calls, .. } if calls == [gate]));
        worker.reply(early, 42);
        worker.reply(gate, 0);
        assert!(
            matches!(worker.read(), WorkerFrame::Delivery { call, verdict: DeliveryVerdict::Accepted, .. } if call == gate)
        );
        if suffix.starts_with("await") {
            assert!(
                matches!(worker.read(), WorkerFrame::Waiting { calls, .. } if calls == [early])
            );
            assert!(
                matches!(worker.read(), WorkerFrame::Delivery { call, verdict: DeliveryVerdict::Accepted, .. } if call == early)
            );
        } else {
            let frame = worker.read();
            assert!(
                matches!(frame, WorkerFrame::Delivery { call, verdict: DeliveryVerdict::NotObserved(_), .. } if call == early),
                "{frame:?}"
            );
        }
        assert!(
            matches!(worker.read(), WorkerFrame::CellDone { outcome: CellOutcome::Completed { value: Some(text), .. }, .. } if text == value)
        );
        worker.send(&HostFrame::Shutdown);
        assert!(matches!(worker.read(), WorkerFrame::ResourceReport { .. }));
    }
}
