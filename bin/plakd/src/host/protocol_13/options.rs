use plakd_wire_types::v13;
use plakous_host_protocol::{views, wire};

pub(super) fn phase(p: wire::RunnerPhase) -> Option<v13::RunnerPhase> {
    use v13::{ReconcileSubphase as S, RunnerPhase as R};
    use wire::RunnerPhase as P;
    Some(match p {
        P::Ssh => R::Ssh,
        P::Daemon => R::Daemon,
        P::Bind => R::Bind,
        P::Options => R::Options,
        P::Discovery => R::Discovery,
        P::Reconcile => return None,
        P::ReconcileStore => R::Reconcile { subphase: S::Store },
        P::ReconcileObsoleteLines => R::Reconcile {
            subphase: S::ObsoleteLines,
        },
        P::ReconcileTerminals => R::Reconcile {
            subphase: S::Terminals,
        },
        P::ReconcileJobs => R::Reconcile { subphase: S::Jobs },
        P::ReconcileStops => R::Reconcile { subphase: S::Stops },
        P::ReconcileForeground => R::Reconcile {
            subphase: S::Foreground,
        },
        P::LinkLost => R::LinkLost,
    })
}

fn placement(reason: wire::PlacementReason) -> Option<v13::PlacementReason> {
    if let wire::PlacementReason::RunnerUnreachable {
        phase: failed,
        timed_out,
    } = reason
    {
        return Some(v13::PlacementReason::Draft(
            v13::PlacementReasonV13::RunnerUnreachable {
                phase: phase(failed)?,
                timed_out,
            },
        ));
    }
    reason.try_into().ok()
}

pub(super) fn refusal(reason: wire::RequestRefusal) -> v13::RequestRefusal {
    match reason.try_into() {
        Ok(shared) => shared,
        Err(wire::RequestRefusal::PlacementRefused { runner, reason }) => {
            match placement(*reason) {
                Some(reason) => {
                    v13::RequestRefusal::Draft(v13::RequestRefusalV13::PlacementRefused {
                        runner,
                        reason: Box::new(reason),
                    })
                }
                None => v13::RequestRefusal::session_unavailable(format!(
                    "runner {}: reconciliation unavailable",
                    runner.name
                )),
            }
        }
        Err(_) => v13::RequestRefusal::session_unavailable("session unavailable".into()),
    }
}

pub(super) fn runner(row: wire::RunnerOptions) -> v13::RunnerOptions {
    v13::RunnerOptions {
        runner: row.runner,
        aliases: row.aliases,
        status: match row.status {
            wire::RunnerStatus::Ready => v13::RunnerStatus::Ready,
            wire::RunnerStatus::Unknown => v13::RunnerStatus::Unknown,
            wire::RunnerStatus::Blocked { reason } => placement(reason)
                .map_or(v13::RunnerStatus::Unknown, |reason| {
                    v13::RunnerStatus::Blocked { reason }
                }),
        },
        checked_at: row.checked_at,
        blocked_since: row.blocked_since,
        roots: row.roots,
        default_workspace: row.default_workspace,
        recent_workspaces: row.recent_workspaces,
        discovered_workspaces: row.discovered_workspaces,
        discovery_truncated: row.discovery_truncated,
    }
}

pub(super) fn options(value: wire::SessionOptions) -> v13::SessionOptions {
    v13::SessionOptions {
        runners: value.runners.into_iter().map(runner).collect(),
        models: value.models,
        profiles: profiles(value.profiles),
        templates: value.templates.into_iter().map(template).collect(),
        workspace: value.workspace.and_then(|value| match value {
            wire::WorkspaceOptions::Valid {
                canonical_path,
                profiles: choices,
                templates,
                models,
            } => Some(v13::WorkspaceOptions::Valid {
                canonical_path,
                profiles: profiles(choices),
                templates: templates.into_iter().map(template).collect(),
                models,
            }),
            wire::WorkspaceOptions::Refused { refusal } => Some(v13::WorkspaceOptions::Refused {
                refusal: placement(refusal)?,
            }),
        }),
        accounts: value.accounts,
    }
}

pub(super) fn tier(tier: Option<&str>) -> Option<v13::ServiceTier> {
    match tier {
        Some("fast" | "priority") => Some(v13::ServiceTier::Fast),
        Some("ultrafast") => Some(v13::ServiceTier::Ultrafast),
        _ => None,
    }
}

pub(super) fn template(row: views::TemplateRow) -> v13::TemplateRow {
    v13::TemplateRow {
        name: row.name,
        account: row.account,
        model: row.model,
        effort: row.effort,
        composition: row.composition,
        service_tier: tier(Some(&row.tier)),
        unready_reason: row.unready_reason,
    }
}

pub(super) fn profiles(view: views::ProfilesView) -> v13::ProfilesView {
    v13::ProfilesView {
        rows: view.rows.into_iter().map(profile).collect(),
        current: view.current,
        diagnostics: view.diagnostics,
    }
}

pub(super) fn profile(row: views::ProfileRow) -> v13::ProfileRow {
    fn role(row: views::ProfileRole) -> v13::ProfileRole {
        v13::ProfileRole {
            label: row.label,
            account: row.account,
            model: row.model,
            effort: row.effort,
            service_tier: tier(row.service_tier.as_deref()),
            depth: row.depth,
        }
    }
    use v13::ProfileDifference as D;
    use views::ProfileDifference as S;
    v13::ProfileRow {
        id: row.id,
        differences: row
            .differences
            .into_iter()
            .filter_map(|difference| {
                Some(match difference {
                    S::RootAccount { session, profile } => D::RootAccount { session, profile },
                    S::RootModel { session, profile } => D::RootModel { session, profile },
                    S::RootEffort { session, profile } => D::RootEffort { session, profile },
                    S::RootFast { .. } => return None,
                    S::RootServiceTier { session } => D::RootServiceTier {
                        session: tier(session.as_deref()),
                    },
                    S::WorkingSet => D::WorkingSet,
                    S::Card(card) => D::Card(card),
                    S::Roster => D::Roster,
                })
            })
            .collect(),
        aliases: row.aliases,
        root: role(row.root),
        cards: row.cards.into_iter().map(role).collect(),
        account_kinds: row.account_kinds,
    }
}
