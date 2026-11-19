use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::CliResult;
use crate::shipping_artifact::{check, fail};

use super::artifacts::Artifacts;
use super::evidence::{self, Log, Pending, Row};
use super::fleet::{self, Fleet};
use super::inventory::{Selected, Stage};
use super::plan::{self, Plan, Step};
use super::protocol::{Canary, ClientOp, Connection, Health, HostOp, Observed};
use super::{Halt, Report, Result};

pub(super) struct Deploy<'a, F> {
    pub(super) artifacts: &'a Artifacts,
    pub(super) config: &'a Selected<'a>,
    pub(super) fleet: F,
    pub(super) stage: Stage,
    pub(super) intent: String,
}

impl<F: Fleet> Deploy<'_, F> {
    pub(super) fn plan(
        &self,
        rows: &[Row],
        log: Option<&mut Log>,
        report: &mut Report,
    ) -> Result<Plan> {
        let prior = evidence::pending(rows)?.map(|(intent, prior)| (intent.to_owned(), prior));
        let observed = match fleet::observed(&self.fleet) {
            Ok(observed) => observed,
            Err(error) if prior.is_some() => {
                return Err(Halt::reconcile(&format!(
                    "recorded upgrade outcome is unresolved: {}",
                    error.message()
                )));
            }
            Err(error) => return Err(error.into()),
        };
        report.put("observed", &observed)?;
        report.put("stage_identity", &observed.stage_identity)?;
        if observed.canary.is_none() && observed.facts.source == "legacy facts" {
            report.put("legacy_facts", &observed.facts)?;
        }
        let routes = plan::qualify(&observed, self.config, self.artifacts, self.stage)?;
        let resume = self.resolve_pending(&observed, prior, log, report)?;
        let mut clients = BTreeSet::new();
        for client in self.config.clients {
            if !self.fleet.client_current(client)? {
                clients.insert(client.id.clone());
            }
        }
        let plan = Plan::new(observed, self.artifacts, clients, routes, resume);
        report.put("plan", &plan.steps)?;
        report.put("source_transition", &plan.source_transition)?;
        report.put("canary_models", &super::receipt::models(&plan.routes))?;
        Ok(plan)
    }

    fn resolve_pending(
        &self,
        observed: &Observed,
        prior: Option<(String, Pending)>,
        log: Option<&mut Log>,
        report: &mut Report,
    ) -> Result<Option<Pending>> {
        if let Some(log) = log {
            log.stage_identity = Some(observed.stage_identity.clone());
            if let Some((intent, prior)) = &prior
                && intent == &self.intent
                && plan::can_withdraw(observed, prior)
            {
                log.record(
                    &self.intent,
                    "withdrawn",
                    &Instance {
                        instance: &prior.instance,
                    },
                )?;
                report.put("withdrawn", &true)?;
                return Ok(None);
            }
        }
        match prior {
            Some((intent, prior)) if intent == self.intent => {
                plan::resume(observed, prior, &self.artifacts.candidate().sha256).map(Some)
            }
            Some(_) => Err(Halt::reconcile(
                "a different recorded upgrade intent still needs reconciliation",
            )),
            None if observed.sha256 != observed.link_sha256 => Err(Halt::reconcile(
                "serving image and stable link differ; reconcile before applying",
            )),
            None => Ok(None),
        }
    }

    pub(super) fn apply(&self, log: &mut Log, report: &mut Report) -> Result<()> {
        let rows = log.rows.clone();
        let mut plan = self.plan(&rows, Some(log), report)?;
        if report.get("withdrawn").and_then(serde_json::Value::as_bool) == Some(true) {
            report.put("result", &"withdrawn")?;
            return Ok(());
        }
        self.production_gate(&mut plan)?;
        let baseline = self.baseline(&plan, report)?;
        if plan.resume.is_some() || !plan.has(Step::StageHost) {
            self.fleet.use_installed_candidate();
        }
        self.stage(&plan, log, report)?;
        let instance = if plan.has(Step::HostUpgrade) {
            self.upgrade(&plan, baseline.as_ref(), log, report)?
        } else {
            plan.observed.instance.clone()
        };
        let ready = self.links(&plan, &instance, log, report)?;
        self.verify(&plan, &ready, baseline.as_ref(), log, report)
    }

    fn production_gate(&self, plan: &mut Plan) -> CliResult<()> {
        if self.stage != Stage::Production
            || plan.has(Step::VerifyReadOnly)
            || plan.resume.is_some()
        {
            return Ok(());
        }
        super::receipt::verify(
            self.config.receipt,
            &self.artifacts.pins,
            &plan.observed.stage_identity,
            &plan.source_transition,
            &super::receipt::models(&plan.routes),
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
                manifest: self.artifacts.raw.clone(),
                manifest_sha256: self.artifacts.pins.host.clone(),
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
        Ok(())
    }

    fn upgrade(
        &self,
        plan: &Plan,
        baseline: Option<&Health>,
        log: &mut Log,
        report: &mut Report,
    ) -> Result<String> {
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
        report.put("step", &"host_upgrade")?;
        let old = &plan.observed;
        check(
            self.observe(report)? == *old,
            "serving facts changed after BASELINE; replan",
        )?;
        let pending = Pending {
            instance: old.instance.clone(),
            source_sha256: old.sha256.clone(),
            source_transition: plan.source_transition.clone(),
            dependencies: old.dependencies(),
            baseline: baseline
                .cloned()
                .ok_or_else(|| fail("upgrade cannot proceed without BASELINE"))?,
        };
        log.record(&self.intent, "upgrade_intent", &pending)?;
        let detached = || {
            Halt::reconcile(
                "host upgrade outcome is unresolved; use deploy status before any new handoff",
            )
        };
        self.fleet
            .host(HostOp::Upgrade {
                candidate: candidate.clone(),
                drain: self.config.drain_seconds,
                previous: Box::new(old.clone()),
            })
            .map_err(|_| detached())?;
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
        let build = &self.artifacts.manifest.build_info;
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
        let transitioned = plan.has(Step::HostUpgrade) || plan.resume.is_some();
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
