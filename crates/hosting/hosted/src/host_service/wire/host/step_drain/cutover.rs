//! Existing exact-turn controls request a yield; only joined results allow surrender.

use super::{
    BTreeMap, BoundaryPhase, BoundaryProgress, CliFailure, CliResult, DriverInstanceId, Duration,
    HostStepDrain, Instant, Ordering, SessionHost, StepDrainMonitor,
};
use plakous::model::TurnId;

#[derive(Default)]
struct Cutover {
    next_yield: BTreeMap<(DriverInstanceId, TurnId), Instant>,
}

impl Cutover {
    async fn yield_tools(&mut self, drain: &HostStepDrain) -> CliResult<()> {
        for (id, driver) in &drain.drivers {
            let progress = driver
                .hold
                .progress()
                .map_err(|error| CliFailure::new(error.to_string()))?;
            let BoundaryProgress::Draining {
                turn: Some(turn),
                phase: BoundaryPhase::Effect,
            } = progress
            else {
                continue;
            };
            let now = Instant::now();
            if self
                .next_yield
                .get(&(*id, turn))
                .is_some_and(|next| now < *next)
            {
                continue;
            }
            self.next_yield
                .insert((*id, turn), now + Duration::from_millis(100));
            let Some(control) = driver.agent.turn_control(turn) else {
                continue;
            };
            // These edges affect admitted dispatches only. A batch can admit
            // another call after an ack, so retry without granting future calls
            // a latch. Refusal or lost ack never proves a joined result.
            // Notice also promotes foreground process waits. Background is
            // deliberately not broadcast: its sticky per-dispatch edge would
            // mask Notice in observation tools that do not accept Background.
            let _ = control.notice().await;
        }
        Ok(())
    }
}

pub(super) async fn finish<P>(
    host: &SessionHost<P>,
    mut drain: HostStepDrain,
    drain_until: Option<Instant>,
    freeze_until: Option<Instant>,
    monitor: &StepDrainMonitor<'_>,
) -> CliResult<HostStepDrain>
where
    P: plakous_host_protocol::ProtocolExtension + 'static,
    P::Intent: Send,
    P::Observation: Send,
    plakous_host_protocol::wire::HostFrame<P>: serde::Serialize,
    plakous_host_protocol::wire::ClientFrame<P>: serde::de::DeserializeOwned,
{
    let mut cutover = Cutover::default();
    let mut next_progress = Instant::now();
    loop {
        if monitor.cancelled.load(Ordering::Acquire) || host.closed() {
            return Err(CliFailure::new(
                "host step drain was cancelled before surrender",
            ));
        }
        let at_bound = drain_until.is_some_and(|deadline| Instant::now() >= deadline);
        let joined = async {
            let effects_joined = drain.refresh().await?;
            if at_bound || drain_until.is_none() {
                cutover.yield_tools(&drain).await?;
            }
            Ok::<_, CliFailure>((effects_joined, drain.pending_boundary(at_bound)?))
        };
        let (effects_joined, pending) =
            join(joined, freeze_until, monitor, || host.closed()).await?;
        if effects_joined && pending.is_none() {
            return Ok(drain);
        }
        if Instant::now() >= next_progress {
            let mut counts = [0_usize; 5];
            let mut active = Vec::new();
            for (id, driver) in &drain.drivers {
                let index = match driver
                    .hold
                    .progress()
                    .map_err(|error| CliFailure::new(error.to_string()))?
                {
                    BoundaryProgress::Held { .. } => 0,
                    BoundaryProgress::Requested => 4,
                    BoundaryProgress::Draining {
                        phase: BoundaryPhase::Model,
                        ..
                    } => 1,
                    BoundaryProgress::Draining {
                        phase: BoundaryPhase::Compaction,
                        ..
                    } => 2,
                    BoundaryProgress::Draining {
                        phase: BoundaryPhase::Effect,
                        ..
                    } => 3,
                };
                counts[index] += 1;
                if index != 0 && active.len() < 8 {
                    active.push(id.to_string());
                }
            }
            (monitor.progress)(&format!(
                "held={} model={} paid_compaction={} effects={} awaiting_boundary={} (also joining tree/owner operations); active drivers: {}",
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                counts[4],
                active.join(",")
            ))?;
            next_progress = Instant::now() + Duration::from_secs(1);
        }
        if freeze_until.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(blocked(
                pending.unwrap_or("tree_or_owner_operation_in_flight"),
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn join<T>(
    joining: impl Future<Output = CliResult<T>>,
    freeze_until: Option<Instant>,
    monitor: &StepDrainMonitor<'_>,
    closed: impl Fn() -> bool,
) -> CliResult<T> {
    let mut joining = std::pin::pin!(joining);
    let mut next_progress = Instant::now() + Duration::from_secs(1);
    loop {
        tokio::select! {
            result = &mut joining => return result,
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        if monitor.cancelled.load(Ordering::Acquire) || closed() {
            return Err(CliFailure::new(
                "host step drain was cancelled before surrender",
            ));
        }
        if freeze_until.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(blocked("joining_tree_or_control_acknowledgement"));
        }
        if Instant::now() >= next_progress {
            (monitor.progress)("joining tree operations and exact-turn yield acknowledgements")?;
            next_progress = Instant::now() + Duration::from_secs(1);
        }
    }
}

fn blocked(reason: &str) -> CliFailure {
    CliFailure::new(format!(
        "host_step_drain_blocked: {reason}; paid compaction, tool results or tree operations did not join within the host-wide freeze budget; source remains authoritative",
    ))
}

#[cfg(test)]
mod tests;
