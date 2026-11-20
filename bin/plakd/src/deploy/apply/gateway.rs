use crate::deploy::evidence::{Cohort, Log, Pending};
use crate::deploy::fleet::{self, Fleet};
use crate::deploy::plan::{self, Plan};
use crate::deploy::protocol::{
    GatewayIntent, GatewayObserved, GatewayOp, GatewayOutcome, Observed, RunnerLink,
};
use crate::deploy::{Halt, Report, Result};
use crate::shipping_artifact::check;

use super::Deploy;

impl<F: Fleet> Deploy<'_, F> {
    pub(super) fn advance_gateway(
        &self,
        plan: &mut Plan,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<()> {
        report.put("step", &"gateway_switch")?;
        let source: GatewayObserved =
            fleet::evidence_decode(self.fleet.gateway(GatewayOp::Observe)?)?;
        check(
            source == plan.observed.gateway,
            "gateway changed after BASELINE; replan",
        )?;
        let intent = GatewayIntent {
            ticket: uuid::Uuid::new_v4(),
            source,
            candidate: self.artifacts.candidate(),
            source_host: plan.observed.sha256.clone(),
            source_instance: plan.observed.instance.clone(),
            target: plan.observed.stage_identity.clone(),
            minimum: plan::gateway_minimum(&plan.routes),
            served: self
                .artifacts
                .host()
                .manifest
                .build_info
                .gateway
                .served
                .clone(),
            accounts: plan.observed.accounts.keys().cloned().collect(),
        };
        pending.gateway = Some(intent.clone());
        self.cohort(log, pending, Cohort::Gateway)?;
        let result = self.gateway_reply(GatewayOp::Switch {
            intent: Box::new(intent),
        })?;
        match result {
            GatewayOutcome::Ready { observed } => {
                plan.observed = self.record_gateway_ready(&observed, pending, log, report)?;
                Ok(())
            }
            GatewayOutcome::Withdrawn => {
                self.withdraw_runner(pending, log, report)?;
                Err(Halt::blocked(
                    "gateway change withdrawn; the source gateway serves and pre-host runner links were restored",
                ))
            }
        }
    }

    pub(super) fn reconcile_gateway(
        &self,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<bool> {
        let intent = pending
            .gateway
            .clone()
            .ok_or_else(|| Halt::reconcile("recorded gateway cohort lacks its target intent"))?;
        self.runner_withdrawal_proof(pending, log, report)?;
        self.fleet.use_installed_candidate();
        report.put("step", &"gateway_reconcile")?;
        match self.gateway_reply(GatewayOp::Reconcile {
            intent: Box::new(intent),
        }) {
            Ok(GatewayOutcome::Ready { observed }) => {
                if pending.gateway_runner_withdrawal {
                    self.restore_runners(pending, report)?;
                }
                if let Err(error) = self.record_gateway_ready(&observed, pending, log, report) {
                    self.withdraw_gateway_runners(pending, log, report)?;
                    return Err(error);
                }
                Ok(true)
            }
            Ok(GatewayOutcome::Withdrawn) => {
                self.withdraw_runner(pending, log, report)?;
                Ok(false)
            }
            Err(error) => {
                self.withdraw_gateway_runners(pending, log, report)?;
                Err(error)
            }
        }
    }

    fn gateway_reply(&self, op: GatewayOp) -> Result<GatewayOutcome> {
        let unresolved = |error: crate::CliFailure| {
            Halt::reconcile(&format!(
                "recorded gateway cohort is unresolved: {}",
                error.message()
            ))
        };
        let reply = self.fleet.gateway(op).map_err(unresolved)?;
        fleet::evidence_decode(reply).map_err(unresolved)
    }

    fn record_gateway_ready(
        &self,
        observed: &GatewayObserved,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<Observed> {
        let intent = pending
            .gateway
            .as_ref()
            .ok_or_else(|| Halt::reconcile("recorded gateway intent is missing"))?;
        let now = self
            .observe(report)
            .map_err(|error| Halt::reconcile(error.message()))?;
        let dependencies = now.dependencies();
        let mut expected = pending.dependencies.clone();
        expected.gateway = dependencies.gateway.clone();
        let destination = RunnerLink {
            path: fleet::deployed(&self.config.host.node, self.artifacts.host()),
            sha256: intent.candidate.sha256.clone(),
        };
        if now.gateway != *observed
            || observed.command != intent.source.command
            || observed.link != destination
            || observed.custody_sha256 != intent.source.custody_sha256
            || observed.holder.instance == intent.source.holder.instance
            || observed.holder.versions != intent.served
            || observed
                .holder
                .negotiated
                .is_none_or(|version| version < intent.minimum)
            || now.accounts.keys().ne(intent.accounts.iter())
            || !Self::same_active_accounts(&now, pending)
            || now.instance != pending.instance
            || now.sha256 != pending.source_sha256
            || now.link_sha256 != pending.source_sha256
            || dependencies != expected
        {
            return Err(Halt::reconcile(
                "recorded gateway successor or incumbent host dependencies did not converge",
            ));
        }
        let mut ready = pending.clone();
        ready.dependencies = dependencies;
        ready.gateway_successor = Some(observed.clone());
        if ready.gateway_runner_withdrawal {
            // Withdrawn runner fences cannot be reused even after the gateway is Ready.
            ready.ticket = uuid::Uuid::new_v4();
            ready.gateway_runner_withdrawal = false;
        }
        report.put("gateway_ready", observed)?;
        self.cohort(log, &mut ready, Cohort::GatewayReady)?;
        *pending = ready;
        Ok(now)
    }

    pub(super) fn has_runner_advance(&self, pending: &Pending) -> bool {
        self.config.runners.iter().any(|runner| {
            pending
                .runners
                .get(&runner.id)
                .is_some_and(|source| source.link != self.runner_destination(runner))
        })
    }

    pub(super) fn withdraw_gateway_runners(
        &self,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<()> {
        if self.has_runner_advance(pending) {
            self.runner_withdrawal_proof(pending, log, report)?;
            if !pending.gateway_runner_withdrawal {
                pending.gateway_runner_withdrawal = true;
                self.cohort(log, pending, Cohort::Gateway)?;
            }
            // Preserve the unresolved gateway intent; the runner withdrawal
            // does not rely on a live credential holder.
            self.restore_runners(pending, report)?;
        }
        Ok(())
    }

    pub(super) fn finish_gateway_runner_withdrawal(
        &self,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<()> {
        self.restore_runners(pending, report)?;
        let mut ready = pending.clone();
        ready.gateway_runner_withdrawal = false;
        ready.ticket = uuid::Uuid::new_v4();
        self.cohort(log, &mut ready, Cohort::GatewayReady)?;
        *pending = ready;
        report.put("withdrawn", &true)?;
        Ok(())
    }
}
