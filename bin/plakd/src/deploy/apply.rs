use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::CliResult;
use crate::shipping_artifact::{check, fail};

use super::artifacts::Artifacts;
use super::evidence::{self, Cohort, Log, Pending, Row};
use super::fleet::{self, Fleet};
use super::inventory::{Runner, Selected, Stage};
use super::plan::{self, Plan, Step, Targets};
use super::protocol::{
    Canary, ClientOp, Connection, Health, HostImages, HostOp, Observed, RunnerLink, RunnerOp,
    StageIdentity,
};
use super::{Halt, Report, Result};

pub(super) struct Deploy<'a, F> {
    pub(super) artifacts: &'a Artifacts,
    pub(super) config: &'a Selected<'a>,
    pub(super) fleet: F,
    pub(super) stage: Stage,
    pub(super) intent: String,
    pub(super) production_evidence: Option<&'a std::path::Path>,
}

impl<F: Fleet> Deploy<'_, F> {
    pub(super) fn plan(
        &self,
        rows: &[Row],
        log: Option<&mut Log>,
        report: &mut Report,
    ) -> Result<Option<Plan>> {
        let prior = evidence::pending(rows)?;
        let mut log = log;
        if let Some((intent, prior)) = &prior {
            if *intent != self.intent {
                return Err(Halt::reconcile(
                    "a different recorded deployment intent still needs reconciliation",
                ));
            }
            if matches!(prior.cohort, Cohort::Runner | Cohort::RunnerWithdrawal) {
                let Some(log) = log else {
                    return Err(Halt::reconcile(
                        "withdraw the recorded runner cohort with deploy status before another handoff",
                    ));
                };
                self.withdraw_runner(&mut prior.clone(), log, report)?;
                return Ok(None);
            }
        }
        let observed = match fleet::observed(&self.fleet) {
            Ok(observed) => observed,
            Err(error) if prior.is_some() => {
                return Err(Halt::reconcile(&format!(
                    "recorded cohort outcome is unresolved: {}",
                    error.message()
                )));
            }
            Err(error) => return Err(error.into()),
        };
        report.put("observed", &observed)?;
        report.put("stage_identity", &observed.stage_identity)?;
        if let Some(log) = &mut log {
            log.stage_identity = Some(observed.stage_identity.clone());
        }
        if let Some((_, pending)) = &prior
            && pending.cohort == Cohort::Host
            && pending.host_upgrade
            && plan::can_withdraw(&observed, pending)
            && let Some(log) = &mut log
        {
            self.withdraw_host(&observed, &mut pending.clone(), log, report)?;
            return Ok(None);
        }
        if observed.canary.is_none() && observed.facts.source == "legacy facts" {
            report.put("legacy_facts", &observed.facts)?;
        }
        let routes = plan::qualify(&observed, self.config, self.artifacts, self.stage)?;
        let mut targets = Targets {
            accounts: BTreeSet::from([observed.stage_identity.account()]),
            runners: BTreeMap::new(),
            runner_changes: BTreeSet::new(),
        };
        let mut clients = BTreeSet::new();
        for client in self.config.clients {
            let found = fleet::client(&self.fleet, client)?;
            self.stage_allowed(found.production_history)?;
            targets.accounts.insert(found.account);
            if !found.current {
                clients.insert(client.id.clone());
            }
        }
        for runner in self.config.runners {
            let found = self.observe_runner(runner)?;
            self.stage_allowed(found.production_history)?;
            targets.accounts.insert(found.account.clone());
            if found.link != self.runner_destination(runner) {
                check(
                    found.link.sha256 == runner.source_sha256,
                    "unexpected dedicated runner link; inspect and re-enroll",
                )?;
                targets.runner_changes.insert(runner.id.clone());
            }
            targets.runners.insert(runner.id.clone(), found);
        }
        report.put("target_accounts", &targets.accounts)?;
        report.put("runner_links", &targets.runners)?;
        self.stage_isolation(&targets.accounts)?;
        let resume =
            self.resolve_pending(&observed, &targets, prior.map(|(_, pending)| pending))?;
        let plan = Plan::new(
            observed,
            targets,
            self.artifacts,
            self.config,
            clients,
            routes,
            resume,
        );
        check(
            plan.has(Step::HostUpgrade) || plan.runner_changes.is_empty(),
            "runner link differs after host handoff; inspect and fix forward",
        )?;
        if plan.has(Step::HostUpgrade) && !self.config.runners.is_empty() {
            self.runner_preflight(&plan.observed)?;
        }
        report.put("plan", &plan.steps)?;
        report.put("source_transition", &plan.source_transition)?;
        report.put("canary_models", &super::receipt::models(&plan.routes))?;
        Ok(Some(plan))
    }

    fn resolve_pending(
        &self,
        observed: &Observed,
        targets: &Targets,
        prior: Option<Pending>,
    ) -> Result<Option<Pending>> {
        match prior {
            Some(prior) => {
                if targets.accounts != prior.accounts
                    || self.config.runners.iter().any(|runner| {
                        let found = &targets.runners[&runner.id];
                        prior.runners.get(&runner.id).is_none_or(|old| {
                            old.account != found.account
                                || (old.link != found.link
                                    && found.link != self.runner_destination(runner))
                        })
                    })
                {
                    return Err(Halt::reconcile(
                        "recorded target account or runner link differs; inspect before continuing",
                    ));
                }
                plan::verify_resume(observed, &prior, &self.artifacts.candidate().sha256)?;
                if prior.cohort == Cohort::Clients
                    && (observed.link_sha256 != observed.sha256
                        || observed.link_sidecars != plan::desired_sidecars(self.artifacts))
                {
                    return Err(Halt::reconcile(
                        "host links no longer match the completed host cohort",
                    ));
                }
                Ok(Some(prior))
            }
            None if observed.sha256 != observed.link_sha256 => Err(Halt::reconcile(
                "serving image and stable link differ; reconcile before applying",
            )),
            None => Ok(None),
        }
    }

    pub(super) fn apply(&self, log: &mut Log, report: &mut Report) -> Result<()> {
        let rows = log.rows.clone();
        let Some(mut plan) = self.plan(&rows, Some(log), report)? else {
            report.put("result", &"withdrawn")?;
            return Ok(());
        };
        self.production_gate(&mut plan, log)?;
        let baseline = self.baseline(&plan, report)?;
        if plan.resume.is_some() || !plan.has(Step::StageHost) {
            self.fleet.use_installed_candidate();
        }
        self.stage(&plan, log, report)?;
        let mut pending = Self::intent_for(&plan, baseline.as_ref());
        let instance = if plan.has(Step::HostUpgrade) {
            let intent = pending
                .as_mut()
                .ok_or_else(|| fail("host cannot proceed without BASELINE"))?;
            self.before_upgrade(&plan, intent, log, report)?;
            self.upgrade(&plan, intent, log, report)?
        } else {
            if let Some(intent) = &mut pending
                && plan.has(Step::HostLink)
                && plan.resume.is_none()
            {
                self.cohort(log, intent, Cohort::Host)?;
            }
            plan.observed.instance.clone()
        };
        let ready = self.links(&plan, &instance, pending.as_mut(), log, report)?;
        self.verify(&plan, &ready, baseline.as_ref(), log, report)
    }

    fn production_gate(&self, plan: &mut Plan, log: &mut Log) -> CliResult<()> {
        if self.stage != Stage::Production
            || plan.has(Step::VerifyReadOnly)
            || plan.resume.is_some()
        {
            return Ok(());
        }
        super::receipt::verify(
            self.config.receipt,
            &self.artifacts.pins,
            &plan.accounts,
            &plan.source_transition,
            &super::receipt::models(&plan.routes),
        )?;
        log.record(
            &self.intent,
            "production_targets",
            &ProductionTargets {
                target_accounts: &plan.accounts,
            },
        )?;
        self.fleet.host(HostOp::MarkProduction {
            expected: plan.observed.stage_identity.clone(),
        })?;
        plan.observed.production_history = true;
        Ok(())
    }

    fn baseline(&self, plan: &Plan, report: &mut Report) -> Result<Option<Health>> {
        let health = if let Some(resume) = &plan.resume {
            resume.baseline.clone()
        } else if plan.has(Step::Baseline) {
            report.put("step", &"baseline")?;
            for route in &plan.routes {
                self.fleet.host(HostOp::CanaryPreflight {
                    expected: plan.observed.sha256.clone(),
                    route: route.clone(),
                })?;
            }
            self.health(plan, &plan.observed, true, "baseline", report)?
        } else {
            return Ok(None);
        };
        report.put("baseline", &health)?;
        Ok(Some(health))
    }

    fn stage(&self, plan: &Plan, log: &mut Log, report: &mut Report) -> Result<()> {
        if plan.has(Step::StageHost) {
            report.put("step", &"stage_host")?;
            self.fleet.host(HostOp::StageHost {
                manifest: self.artifacts.host().raw.clone(),
                manifest_sha256: self.artifacts.host().pin.clone(),
            })?;
            log.record(&self.intent, "stage_host", &())?;
        }
        if plan.has(Step::StageClients) {
            report.put("step", &"stage_clients")?;
            for client in self
                .config
                .clients
                .iter()
                .filter(|client| plan.clients.contains(&client.id))
            {
                self.fleet.client(client, ClientOp::Stage)?;
            }
            log.record(&self.intent, "stage_clients", &())?;
        }
        if plan.has(Step::StageRunners) {
            report.put("step", &"stage_runners")?;
            for runner in self
                .config
                .runners
                .iter()
                .filter(|runner| plan.runner_changes.contains(&runner.id))
            {
                let artifact = self.artifacts.image(runner.node.platform);
                self.fleet.runner(
                    runner,
                    RunnerOp::Stage {
                        manifest: artifact.raw.clone(),
                        manifest_sha256: artifact.pin.clone(),
                    },
                )?;
            }
            log.record(&self.intent, "stage_runners", &())?;
        }
        Ok(())
    }

    fn preflight_destination(&self, plan: &Plan, report: &mut Report) -> Result<()> {
        let candidate = self.artifacts.candidate();
        report.put("step", &"destination_unit")?;
        self.fleet.host(HostOp::DestinationUnit {
            candidate: candidate.clone(),
        })?;
        report.put("step", &"destination_canary_preflight")?;
        for route in &plan.routes {
            self.fleet.host(HostOp::DestinationCanaryPreflight {
                candidate: candidate.clone(),
                route: route.clone(),
            })?;
        }
        Ok(())
    }

    fn intent_for(plan: &Plan, baseline: Option<&Health>) -> Option<Pending> {
        if let Some(prior) = &plan.resume {
            return Some(prior.clone());
        }
        baseline.map(|baseline| Pending {
            cohort: Cohort::Host,
            ticket: uuid::Uuid::new_v4(),
            instance: plan.observed.instance.clone(),
            source_sha256: plan.observed.sha256.clone(),
            source_transition: plan.source_transition.clone(),
            dependencies: plan.observed.dependencies(),
            baseline: baseline.clone(),
            accounts: plan.accounts.clone(),
            runners: plan.runners.clone(),
            host_upgrade: plan.has(Step::HostUpgrade),
        })
    }

    fn cohort(&self, log: &mut Log, pending: &mut Pending, cohort: Cohort) -> CliResult<()> {
        pending.cohort = cohort;
        log.record(&self.intent, cohort.phase(), pending)
    }

    fn before_upgrade(
        &self,
        plan: &Plan,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<()> {
        let mut runner_started = false;
        let preparation: Result<()> = (|| {
            self.preflight_destination(plan, report)?;
            check(
                self.observe(report)? == plan.observed,
                "serving facts changed after BASELINE; replan",
            )?;
            if !self.config.runners.is_empty() {
                self.runner_preflight(&plan.observed)?;
            }
            if plan.has(Step::RunnerLinks) {
                self.cohort(log, pending, Cohort::Runner)?;
                runner_started = true;
                self.advance_runners(plan, pending, report)?;
            }
            // A gateway cohort belongs here, after runner links and before host intent.
            self.verify_runners(plan)?;
            report.put("step", &"host_upgrade")?;
            self.cohort(log, pending, Cohort::Host)?;
            Ok(())
        })();
        if let Err(error) = preparation {
            if runner_started {
                self.withdraw_runner(pending, log, report)?;
                return Err(Halt::blocked(&error.message));
            }
            return Err(error);
        }
        Ok(())
    }

    fn runner_destination(&self, runner: &Runner) -> RunnerLink {
        let artifact = self.artifacts.image(runner.node.platform);
        RunnerLink {
            path: fleet::deployed(&runner.node, artifact),
            sha256: artifact.candidate().sha256,
        }
    }

    fn observe_runner(&self, runner: &Runner) -> CliResult<super::protocol::RunnerObserved> {
        fleet::runner(
            &self.fleet,
            runner,
            self.artifacts.image(runner.node.platform).candidate(),
        )
    }

    fn runner_preflight(&self, observed: &Observed) -> CliResult<()> {
        self.fleet.host(HostOp::RunnerPreflight {
            expected: observed.instance.clone(),
            placements: observed.paired.clone(),
        })?;
        Ok(())
    }

    fn advance_runners(&self, plan: &Plan, pending: &Pending, report: &mut Report) -> Result<()> {
        for runner in self
            .config
            .runners
            .iter()
            .filter(|runner| plan.runner_changes.contains(&runner.id))
        {
            report.put("step", &format!("runner_link_{}", runner.id))?;
            self.fleet.runner(
                runner,
                RunnerOp::Advance {
                    ticket: pending.ticket,
                    candidate: self.artifacts.image(runner.node.platform).candidate(),
                    source: pending.runners[&runner.id].link.clone(),
                },
            )?;
        }
        Ok(())
    }

    fn verify_runners(&self, plan: &Plan) -> Result<()> {
        for runner in self.config.runners {
            let found = self.observe_runner(runner).map_err(link_lost)?;
            self.stage_allowed(found.production_history)?;
            if found.link != self.runner_destination(runner)
                || found.account != plan.runners[&runner.id].account
            {
                return Err(Halt::reconcile(
                    "runner account or dedicated link did not converge",
                ));
            }
        }
        Ok(())
    }

    fn stage_allowed(&self, production_history: bool) -> CliResult<()> {
        check(
            self.stage != Stage::Staging || !production_history,
            "target has production history; use an isolated staging login",
        )
    }

    fn stage_isolation(&self, accounts: &BTreeSet<super::protocol::AccountIdentity>) -> Result<()> {
        if self.stage == Stage::Staging {
            evidence::isolate_staging(accounts, self.production_evidence, self.config.receipt)?;
        }
        Ok(())
    }

    fn withdraw_runner(
        &self,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<()> {
        let observed: HostImages =
            fleet::evidence_decode(self.fleet.host(HostOp::RunnerWithdrawalProof).map_err(
                |_| {
                    Halt::reconcile(
                        "cannot observe the source image and link for runner withdrawal",
                    )
                },
            )?)
            .map_err(link_lost)?;
        report.put("source_images", &observed)?;
        log.stage_identity = Some(observed.stage_identity.clone());
        if observed.sha256 != pending.source_sha256 || observed.link_sha256 != pending.source_sha256
        {
            return Err(Halt::reconcile(
                "cannot restore runner links without the recorded source image and stable link",
            ));
        }
        self.stage_allowed_to_withdraw(
            &observed.stage_identity,
            observed.production_history,
            pending,
        )?;
        if pending.cohort == Cohort::RunnerWithdrawal {
            self.restore_and_record(pending, log, report)
        } else {
            self.begin_runner_withdrawal(pending, log, report)
        }
    }

    fn withdraw_host(
        &self,
        source: &Observed,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<()> {
        self.stage_allowed_to_withdraw(&source.stage_identity, source.production_history, pending)?;
        let detached = || {
            Halt::reconcile(
                "host outcome is unresolved; runner links are unchanged by recovery until the idle source is proven",
            )
        };
        let observed: Observed = fleet::evidence_decode(
            self.fleet
                .host(HostOp::WithdrawUpgrade {
                    ticket: pending.ticket,
                    instance: pending.instance.clone(),
                    source_sha256: pending.source_sha256.clone(),
                })
                .map_err(|_| detached())?,
        )
        .map_err(|_| detached())?;
        if !plan::can_withdraw(&observed, pending) {
            return Err(detached());
        }
        self.begin_runner_withdrawal(pending, log, report)
    }

    fn stage_allowed_to_withdraw(
        &self,
        identity: &StageIdentity,
        production_history: bool,
        pending: &Pending,
    ) -> Result<()> {
        self.stage_allowed(production_history)?;
        let mut accounts = pending.accounts.clone();
        accounts.insert(identity.account());
        self.stage_isolation(&accounts)
    }

    fn begin_runner_withdrawal(
        &self,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<()> {
        self.cohort(log, pending, Cohort::RunnerWithdrawal)?;
        self.restore_and_record(pending, log, report)
    }

    fn restore_and_record(
        &self,
        pending: &Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<()> {
        let mut refusal = None;
        for runner in self.config.runners {
            let old = &pending.runners[&runner.id].link;
            if *old != self.runner_destination(runner)
                && let Err(error) = self.restore_runner(runner, old, pending.ticket, report)
            {
                refusal.get_or_insert(error);
            }
        }
        if let Some(error) = refusal {
            return Err(error);
        }
        log.record(
            &self.intent,
            "withdrawn",
            &Instance {
                instance: &pending.instance,
            },
        )?;
        report.put("withdrawn", &true)?;
        Ok(())
    }

    fn restore_runner(
        &self,
        runner: &Runner,
        old: &RunnerLink,
        ticket: uuid::Uuid,
        report: &mut Report,
    ) -> Result<()> {
        report.put("step", &format!("restore_runner_link_{}", runner.id))?;
        self.fleet
            .runner(
                runner,
                RunnerOp::Restore {
                    ticket,
                    candidate: self.artifacts.image(runner.node.platform).candidate(),
                    source: old.clone(),
                },
            )
            .map_err(link_lost)?;
        if self.observe_runner(runner).map_err(link_lost)?.link != *old {
            return Err(Halt::reconcile(
                "restored runner link did not match the recorded source",
            ));
        }
        Ok(())
    }

    fn upgrade(
        &self,
        plan: &Plan,
        pending: &mut Pending,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<String> {
        let candidate = self.artifacts.candidate();
        let old = &plan.observed;
        let detached = || {
            Halt::reconcile(
                "host upgrade outcome is unresolved; use deploy status before any new handoff",
            )
        };
        if let Err(error) = self.fleet.host(HostOp::Upgrade {
            candidate: candidate.clone(),
            drain: self.config.drain_seconds,
            previous: Box::new(old.clone()),
            ticket: pending.ticket,
        }) {
            if let Ok(observed) = self.observe(report)
                && plan::can_withdraw(&observed, pending)
            {
                self.withdraw_host(&observed, pending, log, report)?;
                return Err(error.into());
            }
            return Err(detached());
        }
        let ready = self.observe(report).map_err(|_| detached())?;
        if ready.sha256 != candidate.sha256
            || ready.instance == old.instance
            || ready.dependencies() != old.dependencies()
        {
            return Err(detached());
        }
        log.record(
            &self.intent,
            "upgrade_ready",
            &Instance {
                instance: &ready.instance,
            },
        )?;
        Ok(ready.instance)
    }

    fn links(
        &self,
        plan: &Plan,
        instance: &str,
        pending: Option<&mut Pending>,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<Observed> {
        let candidate = self.artifacts.candidate();
        if plan.has(Step::HostLink) {
            report.put("step", &"host_link")?;
            self.fleet
                .host(HostOp::HostLink {
                    candidate: candidate.clone(),
                    instance: instance.to_owned(),
                })
                .map_err(link_lost)?;
            log.record(&self.intent, "host_link", &())?;
        }
        let ready = self.observe(report)?;
        let build = &self.artifacts.host().manifest.build_info;
        if ready.instance != instance
            || ready.facts.state_key != build.state_key
            || ready.facts.wire != build.wire
            || ready.sha256 != candidate.sha256
            || ready.link_sha256 != candidate.sha256
            || ready.link_sidecars != plan::desired_sidecars(self.artifacts)
        {
            return Err(Halt::reconcile("target host or link did not converge"));
        }
        check(
            ready.dependencies() == plan.observed.dependencies(),
            "gateway/custody/deployment dependencies changed",
        )?;
        self.verify_runners(plan)?;
        if !plan.clients.is_empty() {
            let pending =
                pending.ok_or_else(|| fail("client links cannot proceed without BASELINE"))?;
            if pending.cohort != Cohort::Clients {
                self.cohort(log, pending, Cohort::Clients)?;
            }
        }
        for client in self.config.clients {
            let connection = || Connection {
                binary_id: ready.facts.binary_id.clone(),
                wire: ready.facts.wire.clone(),
            };
            if plan.clients.contains(&client.id) {
                report.put("step", &format!("client_link_{}", client.id))?;
                self.fleet
                    .client(
                        client,
                        ClientOp::Link {
                            connection: connection(),
                        },
                    )
                    .map_err(link_lost)?;
                log.record(
                    &self.intent,
                    "client_link",
                    &ClientRow { client: &client.id },
                )?;
            }
            report.put("step", &format!("client_check_{}", client.id))?;
            self.fleet.client(
                client,
                ClientOp::Check {
                    connection: connection(),
                },
            )?;
        }
        Ok(ready)
    }

    fn verify(
        &self,
        plan: &Plan,
        ready: &Observed,
        baseline: Option<&Health>,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<()> {
        let paid = plan.has(Step::Verify);
        let health = self.health(plan, ready, paid, "verification", report)?;
        if let Some(baseline) = baseline {
            for (account, old) in &baseline.generations {
                monotonic(*old, health.generations.get(account).copied().flatten())?;
            }
        }
        report.put("verification", &health)?;
        let transitioned = plan.has(Step::HostUpgrade)
            || plan
                .resume
                .as_ref()
                .is_some_and(|intent| intent.host_upgrade);
        let result = if paid { "green" } else { "no_op" };
        report.put("host_transition", &transitioned)?;
        report.put("result", &result)?;
        report.put("completed_at", &evidence::now())?;
        report.remove("step");
        if self.stage == Stage::Staging && transitioned {
            super::receipt::publish(self.config.receipt, report.value())?;
        }
        log.record(&self.intent, "verified", &Verified { result })?;
        Ok(())
    }

    fn health(
        &self,
        plan: &Plan,
        host: &Observed,
        paid: bool,
        label: &str,
        report: &mut Report,
    ) -> Result<Health> {
        report.put("step", &format!("{label}_doctor_status_bindings"))?;
        let mut result: Health = fleet::evidence_decode(self.fleet.host(HostOp::Health {
            expected: host.sha256.clone(),
            routes: plan.routes.clone(),
        })?)?;
        if paid {
            let mut verdicts = BTreeMap::new();
            for route in &plan.routes {
                let account = &route.account;
                report.put("step", &format!("{label}_canary_{account}"))?;
                let reply: Canary = fleet::evidence_decode(self.fleet.host(HostOp::Canary {
                    expected: host.sha256.clone(),
                    route: route.clone(),
                })?)?;
                monotonic(reply.generation_before, reply.generation_after)?;
                let old = result
                    .generations
                    .get(account)
                    .ok_or_else(|| fail("missing account status"))?;
                monotonic(*old, reply.generation_before)?;
                result
                    .generations
                    .insert(account.clone(), reply.generation_after);
                check(
                    reply.adapter == "bounded_cli_v1",
                    "unknown bounded canary adapter",
                )?;
                verdicts.insert(account.clone(), reply.adapter);
            }
            result.canaries = Some(verdicts);
        }
        Ok(result)
    }

    fn observe(&self, report: &mut Report) -> CliResult<Observed> {
        let observed = fleet::observed(&self.fleet)?;
        report.put("observed", &observed)?;
        Ok(observed)
    }
}

fn link_lost(_: crate::CliFailure) -> Halt {
    Halt::reconcile("link outcome is unresolved; use deploy status before continuing")
}

#[derive(Serialize)]
struct ProductionTargets<'a> {
    target_accounts: &'a BTreeSet<super::protocol::AccountIdentity>,
}
fn monotonic(before: Option<u64>, after: Option<u64>) -> CliResult<()> {
    check(
        before.is_none_or(|before| after.is_some_and(|after| after >= before)),
        "OAuth generation regressed",
    )
}

#[derive(Serialize)]
struct Instance<'a> {
    instance: &'a str,
}

#[derive(Serialize)]
struct ClientRow<'a> {
    client: &'a str,
}

#[derive(Serialize)]
struct Verified<'a> {
    result: &'a str,
}
