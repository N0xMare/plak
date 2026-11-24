//! The last source-owned children must not be handed to the new image.

use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use plakous::agent::host_worker_shutdown as workers;
use plakous::tools::host_child_shutdown as groups;

use crate::commands::runner::dial::children::shutdown as runner;

pub(super) fn close() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let runner = start("runner", deadline, runner::seal, runner::poll_reap);
    let evaluators = start("evaluator", deadline, workers::seal, workers::poll_reap);
    let grouped = start(
        "service and Changes",
        deadline,
        || {
            // Allow a short cooperative close before the group sweep. Even
            // incomplete starts, idle eviction and cancelled closes are tracked.
            let grace = Duration::from_millis(250);
            let (sender, receiver) = sync_channel(1);
            let _ = std::thread::Builder::new()
                .name("plak-stop-mcp".into())
                .spawn(move || {
                    let _ = sender.send(plakous_hosted::mcp_binding::shutdown_mcp_pool(grace));
                });
            if !matches!(receiver.recv_timeout(grace), Ok(true)) {
                tracing::warn!(target: "plak.host", "host_mcp_close_incomplete");
            }
            groups::seal();
        },
        groups::poll_reap,
    );

    // A pending spawn or group scan can hold a lock. Wait on the workers'
    // reports instead of contending for that lock on the exec thread.
    for (kind, report) in [
        ("runner", runner),
        ("evaluator", evaluators),
        ("service and Changes", grouped),
    ] {
        match report.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(0) => {}
            Ok(unreaped) => {
                tracing::warn!(target: "plak.host", kind, unreaped, "host_child_reap_incomplete");
            }
            Err(_) => tracing::warn!(target: "plak.host", kind, "host_child_reap_incomplete"),
        }
    }
}

fn start(
    kind: &str,
    deadline: Instant,
    seal: impl FnOnce() + Send + 'static,
    poll: impl FnMut() -> usize + Send + 'static,
) -> Receiver<usize> {
    let (sender, receiver) = sync_channel(1);
    let _ = std::thread::Builder::new()
        .name(format!("plak-stop-{kind}"))
        .spawn(move || {
            seal();
            let _ = sender.send(reap_until(deadline, poll));
        });
    receiver
}

fn reap_until(deadline: Instant, mut poll: impl FnMut() -> usize) -> usize {
    loop {
        let remaining = poll();
        let time = deadline.saturating_duration_since(Instant::now());
        if remaining == 0 || time.is_zero() {
            return remaining;
        }
        std::thread::sleep(time.min(Duration::from_millis(5)));
    }
}
