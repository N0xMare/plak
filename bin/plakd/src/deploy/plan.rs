use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::CliResult;
use crate::shipping_artifact::{NAMES, check, fail};

use super::Halt;
use super::artifacts::Artifacts;
use super::evidence::Pending;
use super::inventory::{Route, Selected, Stage};
use super::protocol::{Observed, SourceTransition};

pub(super) const BOOTSTRAP: &str = "bootstrap_required: upgrade the pre-S2 incumbent using the existing manual same-key procedure (stage with install.sh --no-link, run the destination's host upgrade --expect-binary <id> --drain <duration>, then run install.sh link <snapshot-id> after Ready); re-run deploy plan with an S2 incumbent";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Step {
    Baseline,
    StageHost,
    StageClients,
    HostUpgrade,
    HostLink,
    ClientLinks,
    Verify,
    VerifyReadOnly,
}

pub(super) struct Plan {
    pub(super) observed: Observed,
    pub(super) clients: BTreeSet<String>,
    pub(super) routes: Vec<Route>,
    pub(super) steps: Vec<Step>,
    pub(super) resume: Option<Pending>,
    pub(super) source_transition: SourceTransition,
}

impl Plan {
    pub(super) fn new(
        observed: Observed,
        artifacts: &Artifacts,
        clients: BTreeSet<String>,
        routes: Vec<Route>,
        resume: Option<Pending>,
    ) -> Self {
        let desired = artifacts.candidate();
        let sidecars = desired_sidecars(artifacts);
        let host_change = observed.sha256 != desired.sha256;
        let host_link =
            observed.link_sha256 != desired.sha256 || observed.link_sidecars != sidecars;
        let client_change = !clients.is_empty();
        let mut steps = Vec::new();
        if resume.is_none() && (host_change || host_link || client_change) {
            steps.push(Step::Baseline);
        }
        if host_change {
            steps.push(Step::StageHost);
        }
        if client_change {
            steps.push(Step::StageClients);
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
            || observed.source_transition(),
            |prior| prior.source_transition.clone(),
        );
        Self {
            observed,
            clients,
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

pub(super) fn desired_sidecars(artifacts: &Artifacts) -> BTreeMap<String, String> {
    NAMES
        .into_iter()
        .skip(1)
        .map(|name| {
            (
                name.to_owned(),
                artifacts.manifest.files[name].sha256.clone(),
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
    let want = &artifacts.manifest.build_info;
    check(
        stage != Stage::Staging || !observed.production_history,
        "target has production history; use an isolated staging host",
    )?;
    check(
        observed.facts.state_key == want.state_key,
        "cross-key: use the settled host upgrade runbook",
    )?;
    let triple = &observed.facts.runner_triple;
    if observed.paired
        && (triple.protocol != want.runner.protocol
            || triple.source_revision != want.revision
            || triple.flavor != "release")
    {
        return Err(fail(
            "paired-runner triple change: settle and qualify the old placements; use S3",
        )
        .into());
    }
    check(
        !observed.paired,
        "paired runner placement is outside S2; use S3",
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
    let minimum = if active
        .iter()
        .any(|route| matches!(route.kind(), "codex" | "grok"))
    {
        2
    } else {
        1
    };
    check(
        observed
            .gateway_versions
            .iter()
            .any(|version| *version >= minimum && want.gateway.spoken.contains(version)),
        &format!("in-use accounts require unchanged gateway protocol v{minimum} or newer; use S3"),
    )?;
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
        && observed.sha256 == observed.link_sha256
        && observed.upgrade_idle
        && observed.dependencies() == prior.dependencies
}

pub(super) fn resume(
    observed: &Observed,
    prior: Pending,
    candidate: &str,
) -> super::Result<Pending> {
    if observed.sha256 == candidate
        && observed.instance != prior.instance
        && observed.dependencies() == prior.dependencies
    {
        Ok(prior)
    } else {
        Err(Halt::reconcile(
            "recorded upgrade outcome is unresolved; inspect the serving coordinator and unit before any new handoff",
        ))
    }
}
