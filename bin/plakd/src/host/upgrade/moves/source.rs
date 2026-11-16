//! Live membership is inspected before drain; only joined cuts become capsules.
use super::super::{bindings, step_drain};
use super::{CliFailure, CliResult, Request, Services, preflight};
use crate::carry::host_move::{
    live_protocol,
    source::{Inventory, Selection},
};
use crate::surface_protocol::PlakExtension;
use plakous::sessions::host_move::MoveJobCut;
use plakous_hosted::host_service::wire::{AdmissionPause, HostStepDrain, SessionHost};
use std::sync::{Arc, atomic::AtomicBool};

pub(super) struct Prepared {
    pub(super) connections: Vec<plakous::runner::RunnerConnectionHold>,
    pub(super) drain: Option<HostStepDrain>,
    pub(super) inventory: Option<Inventory>,
    pub(super) standalone: std::fs::File,
    pub(super) pause: AdmissionPause,
}

pub(super) fn prepare(
    host: &Arc<SessionHost<PlakExtension>>,
    services: &Services,
    request: &Request,
    cancelled: &Arc<AtomicBool>,
    runtime: &tokio::runtime::Handle,
    progress: &(dyn Fn(&str) -> CliResult<()> + Sync),
    operation: [u8; 32],
) -> CliResult<Prepared> {
    let paths = services.paths()?;
    let live = live_protocol().is_some();
    let selection = Selection::read(services, operation)?;
    let mut connections = Vec::new();
    if let Some(selection) = &selection {
        if live {
            connections = super::retain_runners(services, selection.runners(), runtime)?;
            bindings::preflight_selected(paths, &request.destination, &selection.sessions())?;
            crate::carry::upgrade::crash_cut("move-after-live-inspection");
        } else if let Some(inventory) = Inventory::read(services, operation)? {
            connections = super::retain_connections(services, &inventory, runtime)?;
            preflight(services, request, &inventory, cancelled, runtime, progress)?;
        }
    }
    let standalone = crate::carry::upgrade::admission(paths, true)?;
    let pause = host.pause_admissions_for_upgrade(Arc::clone(cancelled))?;
    let budget = step_drain::Budget::new(request.drain)?;
    host.resolve_paused_drafts(&pause, budget.barrier_deadline, cancelled)?;
    let drain = live
        .then(|| step_drain::prepare(host, services, &pause, budget, cancelled, runtime, progress))
        .transpose()?;
    if live {
        crate::carry::upgrade::crash_cut("move-after-live-gates");
    }
    if !host
        .hand_off_paused(
            &pause,
            super::super::super::HANDOFF_DEADLINE.saturating_sub(super::super::super::STOP_MARGIN),
        )?
        .is_empty()
    {
        return Err(CliFailure::new(
            "host_upgrade_drain_failed: no connected group surrendered",
        ));
    }
    if live {
        crate::carry::upgrade::crash_cut("move-after-live-handoff");
    }
    let job_cut = if live {
        MoveJobCut::Retained
    } else {
        MoveJobCut::Settled
    };
    let inventory = Inventory::read_cut(services, operation, job_cut)?;
    if selection.as_ref().map(|selection| &selection.groups)
        != inventory.as_ref().map(|inventory| &inventory.groups)
    {
        return Err(CliFailure::new(
            "host_upgrade_inventory_changed: source membership changed during preflight; retry without surrender",
        ));
    }
    Ok(Prepared {
        connections,
        drain,
        inventory,
        standalone,
        pause,
    })
}
