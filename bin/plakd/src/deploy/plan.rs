use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::CliResult;
use crate::shipping_artifact::{NAMES, check};

use super::Halt;
use super::artifacts::Artifacts;
use super::evidence::Pending;
use super::inventory::{Route, Selected, Stage};
use super::protocol::{
    AccountIdentity, Observed, RunnerObserved, RunnerPlacement, RunnerTransition, RunnerTriple,
    SourceTransition,
};

pub(super) const BOOTSTRAP: &str = "bootstrap_required: upgrade the pre-S2 incumbent using the existing manual same-key procedure (stage with install.sh --no-link, run the destination's host upgrade --expect-binary <id> --drain <duration>, then run install.sh link <snapshot-id> after Ready); re-run deploy plan with an S2 incumbent";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Step {
    Baseline,
    StageHost,
    StageClients,
    StageRunners,
    RunnerLinks,
    Gateway,
    HostUpgrade,
    HostLink,
    ClientLinks,
    Verify,
    VerifyReadOnly,
}

pub(super) struct Plan {
    pub(super) observed: Observed,
    pub(super) clients: BTreeSet<String>,
    pub(super) runners: BTreeMap<String, RunnerObserved>,
    pub(super) runner_changes: BTreeSet<String>,
    pub(super) accounts: BTreeSet<AccountIdentity>,
    pub(super) routes: Vec<Route>,
    pub(super) steps: Vec<Step>,
    pub(super) resume: Option<Pending>,
    pub(super) source_transition: SourceTransition,
}

impl Plan {
    pub(super) fn new(
        observed: Observed,
        targets: Targets,
        artifacts: &Artifacts,
        configuration: &Selected<'_>,
        clients: BTreeSet<String>,
        routes: Vec<Route>,
        resume: Option<Pending>,
    ) -> Self {
        let desired = artifacts.candidate();
        let sidecars = desired_sidecars(artifacts);
        let host_change = observed.sha256 != desired.sha256;
        let gateway_change = observed.gateway.link.sha256 != desired.sha256;
        let host_link =
            observed.link_sha256 != desired.sha256 || observed.link_sidecars != sidecars;
        let client_change = !clients.is_empty();
        let runner_change = !targets.runner_changes.is_empty();
        let mut steps = Vec::new();
        if resume.is_none()
            && (host_change || gateway_change || host_link || client_change || runner_change)
        {
            steps.push(Step::Baseline);
        }
        if (host_change || gateway_change) && resume.is_none() {
            steps.push(Step::StageHost);
        }
        if client_change {
            steps.push(Step::StageClients);
        }
        if runner_change && resume.is_none() {
            steps.push(Step::StageRunners);
        }
        if runner_change {
            steps.push(Step::RunnerLinks);
        }
        if gateway_change {
            steps.push(Step::Gateway);
        }
        if host_change {
            steps.push(Step::HostUpgrade);
        }
        if host_link {
            steps.push(Step::HostLink);
        }
        if client_change {
            steps.push(Step::ClientLinks);
        }
        steps.push(if steps.is_empty() && resume.is_none() {
            Step::VerifyReadOnly
        } else {
            Step::Verify
        });
        let source_transition = resume.as_ref().map_or_else(
            || {
                let mut transition = observed.source_transition();
                transition.host_changed = host_change;
                transition.gateway_changed = gateway_change;
                transition.runners = configuration
                    .runners
                    .iter()
                    .map(|runner| {
                        let own = &targets.runners[&runner.id].runner_triple;
                        let image = &artifacts.image(runner.node.platform).manifest.build_info;
                        let destination = RunnerTriple {
                            protocol: image.runner.protocol,
                            source_revision: image.revision.clone(),
                            flavor: image.flavor.clone(),
                        };
                        let class = if !targets.runner_changes.contains(&runner.id) {
                            RunnerTransition::Unchanged
                        } else if *own == destination {
                            RunnerTransition::SameTriple
                        } else {
                            RunnerTransition::ChangedTriple
                        };
                        (runner.node.platform, class)
                    })
                    .collect();
                transition.runners.sort_unstable();
                transition
            },
            |prior| prior.source_transition.clone(),
        );
        Self {
            observed,
            clients,
            runners: targets.runners,
            runner_changes: targets.runner_changes,
            accounts: targets.accounts,
            routes,
            steps,
            resume,
            source_transition,
        }
    }

    pub(super) fn has(&self, step: Step) -> bool {
        self.steps.contains(&step)
    }
}

pub(super) struct Targets {
    pub(super) accounts: BTreeSet<AccountIdentity>,
    pub(super) runners: BTreeMap<String, RunnerObserved>,
    pub(super) runner_changes: BTreeSet<String>,
}

pub(super) fn desired_sidecars(artifacts: &Artifacts) -> BTreeMap<String, String> {
    NAMES
        .into_iter()
        .skip(1)
        .map(|name| {
            (
                name.to_owned(),
                artifacts.host().manifest.files[name].sha256.clone(),
            )
        })
        .collect()
}

pub(super) fn qualify(
    observed: &Observed,
    configuration: &Selected<'_>,
    artifacts: &Artifacts,
    stage: Stage,
) -> super::Result<Vec<Route>> {
    let want = &artifacts.host().manifest.build_info;
    check(
        stage != Stage::Staging || !observed.production_history,
        "target has production history; use an isolated staging host",
    )?;
    check(
        observed.facts.state_key == want.state_key,
        "cross-key: use the settled host upgrade runbook",
    )?;
    let triple = &observed.facts.runner_triple;
    let placements: BTreeMap<_, _> = configuration
        .runners
        .iter()
        .map(|runner| {
            (
                runner.id.clone(),
                RunnerPlacement {
                    ssh: format!("{}@{}", runner.node.login, runner.node.ssh),
                    command: runner.link.clone(),
                },
            )
        })
        .collect();
    check(
        observed.paired == placements,
        "paired runner placements must match inventory IDs, login@host and dedicated command paths",
    )?;
    check(
        triple.protocol == want.runner.protocol,
        "home-local runner protocol change: settle jobs and use the runner upgrade runbook",
    )?;
    check(
        observed.facts.host_upgrade == want.host_upgrade,
        "coordinator/receipt change: enroll a compatible incumbent first",
    )?;
    let active = routes(observed, configuration)?;
    qualify_gateway(observed, configuration, artifacts, gateway_minimum(&active))?;
    let candidate = artifacts.candidate();
    let source = &configuration.host.source_sha256;
    check(
        [source, &candidate.sha256].contains(&&observed.sha256),
        "unexpected serving image; inspect and re-enroll source",
    )?;
    check(
        [source, &candidate.sha256].contains(&&observed.link_sha256),
        "unexpected stable link; inspect and re-enroll source",
    )?;
    let Some(supported) = &observed.canary else {
        return Err(Halt::bootstrap());
    };
    let missing = active
        .iter()
        .map(Route::kind)
        .filter(|kind| !supported.iter().any(|val| val == kind))
        .collect::<BTreeSet<_>>();
    check(
        missing.is_empty(),
        &format!(
            "no bounded deploy canary for {}; use the manual same-key procedure for this fleet",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        ),
    )?;
    if observed.sha256 != candidate.sha256 {
        artifacts.check_ancestor(&triple.source_revision)?;
    }
    Ok(active)
}

fn qualify_gateway(
    observed: &Observed,
    configuration: &Selected<'_>,
    artifacts: &Artifacts,
    minimum: u32,
) -> CliResult<()> {
    let want = &artifacts.host().manifest.build_info;
    check(
        observed
            .gateway
            .holder
            .versions
            .iter()
            .any(|version| *version >= minimum && want.gateway.spoken.contains(version)),
        &format!("in-use accounts require gateway protocol v{minimum} or newer"),
    )?;
    let candidate = artifacts.candidate();
    let gateway = &observed.gateway;
    let destination = super::fleet::deployed(&configuration.host.node, artifacts.host());
    check(
        if gateway.link.sha256 == candidate.sha256 {
            gateway.link.path == destination
        } else {
            gateway.link.sha256 == configuration.host.gateway_sha256
        },
        "unexpected gateway image or dedicated link; inspect and re-enroll source",
    )?;
    if gateway.link.sha256 != candidate.sha256 {
        check(
            gateway
                .holder
                .negotiated
                .is_some_and(|version| want.gateway.served.contains(&version)),
            "the new gateway must serve the incumbent host's negotiated protocol",
        )?;
        check(
            want.gateway.served.iter().any(|version| {
                *version >= minimum && observed.facts.gateway_spoken.contains(version)
            }) && want
                .gateway
                .served
                .iter()
                .any(|version| *version >= minimum && want.gateway.spoken.contains(version)),
            &format!("the new gateway cannot negotiate protocol v{minimum} with both hosts"),
        )?;
    }
    Ok(())
}

pub(super) fn gateway_minimum(routes: &[Route]) -> u32 {
    if routes
        .iter()
        .any(|route| matches!(route.kind(), "codex" | "grok"))
    {
        2
    } else {
        1
    }
}

fn routes(observed: &Observed, configuration: &Selected<'_>) -> CliResult<Vec<Route>> {
    let mut active = Vec::new();
    let mut missing = Vec::new();
    for (account, status) in &observed.accounts {
        if status == "not_enrolled" {
            continue;
        }
        if let Some(route) = configuration
            .routes
            .iter()
            .find(|route| &route.account == account)
        {
            active.push(route.clone());
        } else {
            missing.push(account.as_str());
        }
    }
    check(
        missing.is_empty(),
        &format!(
            "in-use accounts missing from inventory: {}",
            missing.join(", ")
        ),
    )?;
    Ok(active)
}

pub(super) fn can_withdraw(observed: &Observed, prior: &Pending) -> bool {
    observed.instance == prior.instance
        && observed.sha256 == prior.source_sha256
        && observed.link_sha256 == prior.source_sha256
        && observed.upgrade_idle
        && observed.dependencies() == prior.dependencies
}

pub(super) fn verify_resume(
    observed: &Observed,
    prior: &Pending,
    candidate: &str,
) -> super::Result<()> {
    let identity = if prior.host_upgrade && prior.cohort != super::evidence::Cohort::GatewayReady {
        observed.instance != prior.instance && observed.sha256 == candidate
    } else {
        observed.instance == prior.instance
            && observed.sha256 == prior.source_sha256
            && (prior.cohort != super::evidence::Cohort::GatewayReady
                || observed.link_sha256 == prior.source_sha256)
    };
    if identity && observed.dependencies() == prior.dependencies {
        Ok(())
    } else {
        Err(Halt::reconcile(
            "recorded upgrade outcome is unresolved; inspect the serving coordinator and unit before any new handoff",
        ))
    }
}
