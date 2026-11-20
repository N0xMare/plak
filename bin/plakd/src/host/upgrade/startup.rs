//! Host-owned imports reuse registry connections and acknowledge fresh reconciliation.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::carry::upgrade::{
    crash_cut, importing,
    receipt::{self, Phase, Receipt},
};
use crate::commands::runner::services::host as runner;
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};

pub(in crate::host) fn prepare_host(
    services: &Services,
) -> CliResult<(Services, Arc<plakous::runner::WorkspaceCatalog>)> {
    for receipt in pending(services)? {
        if !receipt.groups.is_empty()
            && let Err(failure) = finish_source(services, &receipt)
        {
            deferred("source retirement", &failure);
        }
    }
    let services = runner::custody(services)?;
    for mut receipt in pending(&services)? {
        crash_cut("before-import");
        if !receipt.groups.is_empty() {
            let _lock = receipt::publication::lock(services.paths()?)?;
            for index in 0..receipt.groups.len() {
                let group = &receipt.groups[index];
                if group.phase != receipt::GroupPhase::Ready
                    && group.phase != receipt::GroupPhase::Abandoned
                    && group.runners.iter().all(|runner| runner == "local")
                    && let Err(failure) = crate::carry::host_move::installation::install(
                        &services,
                        &mut receipt,
                        index,
                    )
                {
                    deferred("group", &failure);
                }
            }
            continue;
        }
        import_local_mappings(&services, &receipt);
    }
    runner::prepare(&services, None)
}

fn import_local_mappings(services: &Services, receipt: &Receipt) {
    for mapping in receipt
        .mappings
        .iter()
        .filter(|mapping| !mapping.ready && !mapping.abandoned && mapping.runner == "local")
    {
        if let Err(failure) = importing::one(services, receipt, mapping) {
            deferred(&mapping.successor, &failure);
        }
    }
}

fn pending(services: &Services) -> CliResult<Vec<Receipt>> {
    Ok(receipt::all(services.paths()?)?
        .into_iter()
        .filter(|receipt| {
            receipt.destination_key == crate::home::state::state_key()
                && receipt.destination_binary == crate::build_identity::binary_id()
                && receipt.phase == Phase::Committed
        })
        .collect())
}

fn finish_source(services: &Services, receipt: &Receipt) -> CliResult<()> {
    if receipt.groups.iter().all(|group| group.source_retired) {
        return Ok(());
    }
    // The producer takes the publication lock itself; never call it under that lock.
    super::process::verify_image(&receipt.source_executable, &receipt.source_binary)?;
    let mut command = super::process::command(
        &receipt.source_executable,
        services.paths()?.home(),
        &receipt.source_key,
    );
    command
        .args(["version", "--host-upgrade-finish-source"])
        .arg(receipt.id.to_string());
    super::process::run(command, Duration::from_secs(30))?;
    Ok(())
}

fn deferred(successor: &str, failure: &CliFailure) {
    eprintln!(
        "host upgrade successor {successor} remains withheld; destination recovery will retry: {}",
        crate::cli_output::safe_terminal_block(failure.message())
    );
}

pub(in crate::host) struct Recovery {
    stop: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Recovery {
    pub(in crate::host) fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake.notify_one();
    }
    pub(in crate::host) fn join(mut self) {
        self.finish();
    }
    fn finish(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Recovery {
    fn drop(&mut self) {
        self.finish();
    }
}

pub(in crate::host) fn start_retry(
    services: &Services,
    host: &Arc<
        plakous_hosted::host_service::wire::SessionHost<crate::surface_protocol::PlakExtension>,
    >,
    runtime: &tokio::runtime::Handle,
    initial_owed: &[String],
    scanning: Arc<std::sync::atomic::AtomicUsize>,
) -> Recovery {
    let stop = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::clone(&stop);
    let wake = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&wake);
    let services = services.clone();
    let host = Arc::clone(host);
    let runtime = runtime.clone();
    let mut scheduled = initial_owed
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let worker = std::thread::spawn(move || {
        let mut changes = services.runners().changes();
        let mut delay = Duration::from_secs(5);
        while !cancelled.load(Ordering::Acquire) {
            let receipts = match pending(&services) {
                Ok(receipts) if receipts.is_empty() => break,
                Ok(receipts) => receipts,
                Err(failure) => {
                    deferred("receipt", &failure);
                    break;
                }
            };
            for receipt in receipts {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                let path = match services.paths() {
                    Ok(paths) => receipt.path(paths),
                    Err(failure) => {
                        deferred("receipt", &failure);
                        return;
                    }
                };
                if let Err(failure) = advance(&services, &runtime, receipt, &cancelled, &host) {
                    deferred("receipt", &failure);
                }
                match receipt::read(&path).and_then(|receipt| {
                    resume_ready(
                        &services,
                        &host,
                        &runtime,
                        &receipt,
                        &cancelled,
                        &scanning,
                        &mut scheduled,
                    )
                }) {
                    Ok(()) => {}
                    Err(failure) => deferred("group continuation", &failure),
                }
                host.changed();
            }
            let status_transition = runtime.block_on(wait(&services, &mut changes, &notify, delay));
            delay = if status_transition {
                Duration::from_secs(5)
            } else {
                delay.saturating_mul(2).min(Duration::from_secs(180))
            };
        }
    });
    Recovery {
        stop,
        wake,
        worker: Some(worker),
    }
}

fn resume_ready(
    services: &Services,
    host: &Arc<
        plakous_hosted::host_service::wire::SessionHost<crate::surface_protocol::PlakExtension>,
    >,
    runtime: &tokio::runtime::Handle,
    receipt: &Receipt,
    stop: &Arc<AtomicBool>,
    scanning: &Arc<std::sync::atomic::AtomicUsize>,
    scheduled: &mut std::collections::BTreeSet<String>,
) -> CliResult<()> {
    let mut owed = Vec::new();
    for product in receipt
        .groups
        .iter()
        .filter(|group| group.phase == receipt::GroupPhase::Ready)
        .flat_map(|group| &group.products)
    {
        if scheduled.contains(product) {
            continue;
        }
        if super::super::sessions::continuation_owed(services, product)? {
            owed.push(product.clone());
        }
        scheduled.insert(product.clone());
    }
    let stop = Arc::clone(stop);
    super::super::scan(
        host,
        owed,
        move || stop.load(Ordering::Acquire),
        services,
        runtime,
        Arc::clone(scanning),
    );
    Ok(())
}

async fn wait(
    services: &Services,
    changes: &mut tokio::sync::watch::Receiver<u64>,
    wake: &tokio::sync::Notify,
    delay: Duration,
) -> bool {
    let statuses = || {
        services
            .runners()
            .rows()
            .into_iter()
            .map(|row| (row.runner.runner_id, row.status))
            .collect::<Vec<_>>()
    };
    let before = statuses();
    changes.borrow_and_update();
    if statuses() != before {
        return true;
    }
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            () = wake.notified() => return false,
            () = tokio::time::sleep_until(deadline) => return false,
            result = changes.changed() => {
                if result.is_err() || statuses() != before { return true; }
            }
        }
    }
}

fn advance(
    services: &Services,
    runtime: &tokio::runtime::Handle,
    receipt: Receipt,
    stop: &AtomicBool,
    host: &plakous_hosted::host_service::wire::SessionHost<crate::surface_protocol::PlakExtension>,
) -> CliResult<()> {
    if !receipt.groups.is_empty() {
        finish_source(services, &receipt)?;
        return advance_groups(services, &receipt, stop, host);
    }
    advance_mappings(services, runtime, receipt, stop, host)
}

fn advance_groups(
    services: &Services,
    receipt: &Receipt,
    stop: &AtomicBool,
    host: &plakous_hosted::host_service::wire::SessionHost<crate::surface_protocol::PlakExtension>,
) -> CliResult<()> {
    for index in 0..receipt.groups.len() {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let _lock = receipt::publication::lock(services.paths()?)?;
        let mut latest = receipt::read(&receipt.path(services.paths()?))?;
        let group = &latest.groups[index];
        if matches!(
            group.phase,
            receipt::GroupPhase::Ready | receipt::GroupPhase::Abandoned
        ) {
            continue;
        }
        if group.phase == receipt::GroupPhase::Abandoning {
            if !group.abandon_retry_due() {
                continue;
            }
            if let Err(failure) =
                crate::carry::host_move::disposition::abandon(services, &mut latest, index)
            {
                deferred("group abandon", &failure);
            }
            host.changed();
            continue;
        }
        for runner in &group.runners {
            let required = services.runners().needs_connection(runner);
            services.runners().require_connection(runner, true);
            let ready = services.runners().rows().iter().any(|row| {
                &row.runner.runner_id == runner
                    && row.status == plakous::runner::RunnerStatus::Ready
            });
            if (!required && !ready) || (ready && services.runners().services(runner).is_err()) {
                services.runners().reconcile_when_available(runner);
            }
        }
        if group.runners.iter().any(|runner| {
            !services.runners().rows().iter().any(|row| {
                &row.runner.runner_id == runner
                    && row.status == plakous::runner::RunnerStatus::Ready
            })
        }) {
            continue;
        }
        if let Err(failure) =
            crate::carry::host_move::installation::install(services, &mut latest, index)
        {
            deferred("group", &failure);
            continue;
        }
        if let Err(failure) =
            crate::carry::host_move::reconciliation::ready(services, &mut latest, index)
        {
            deferred("group", &failure);
            continue;
        }
        for runner in &latest.groups[index].runners {
            if latest
                .groups
                .iter()
                .filter(|group| group.runners.contains(runner))
                .all(|group| {
                    matches!(
                        group.phase,
                        receipt::GroupPhase::Ready | receipt::GroupPhase::Abandoned
                    )
                })
            {
                services.runners().require_connection(runner, false);
            }
        }
        host.changed();
    }
    Ok(())
}

fn advance_mappings(
    services: &Services,
    runtime: &tokio::runtime::Handle,
    receipt: Receipt,
    stop: &AtomicBool,
    host: &plakous_hosted::host_service::wire::SessionHost<crate::surface_protocol::PlakExtension>,
) -> CliResult<()> {
    let paths = services.paths()?;
    let mut order = (0..receipt.mappings.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| receipt.mappings[*index].runner != "local");
    for index in order {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let _lock = receipt::publication::lock(paths)?;
        let mut latest = receipt::read(&receipt.path(paths))?;
        let mapping = &latest.mappings[index];
        if latest.phase != Phase::Committed || mapping.ready || mapping.abandoned {
            continue;
        }
        let id = mapping.runner.clone();
        let required = services.runners().needs_connection(&id);
        services.runners().require_connection(&id, true);
        if !services.runners().rows().iter().any(|row| {
            row.runner.runner_id == id && row.status == plakous::runner::RunnerStatus::Ready
        }) {
            if !required {
                services.runners().reconcile_when_available(&id);
            }
            continue;
        }
        match importing::one(services, &latest, mapping) {
            Ok(_) => {}
            Err(failure) => {
                deferred(&mapping.successor, &failure);
                continue;
            }
        }
        crash_cut("after-import");
        services.runners().reconcile_when_available(&id);
        let refreshed = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(25),
                services.runners().refresh(Some(std::slice::from_ref(&id))),
            )
            .await
        });
        if !matches!(refreshed, Ok(Ok(_)))
            || !services.runners().rows().iter().any(|row| {
                row.runner.runner_id == id && row.status == plakous::runner::RunnerStatus::Ready
            })
        {
            continue;
        }
        crash_cut("before-ready");
        latest.mappings[index].ready = true;
        acknowledge(&mut latest);
        latest.save(paths)?;
        if latest
            .mappings
            .iter()
            .filter(|mapping| mapping.runner == id)
            .all(|mapping| mapping.ready || mapping.abandoned)
        {
            services.runners().require_connection(&id, false);
        }
        host.changed();
    }
    let _lock = receipt::publication::lock(paths)?;
    let mut latest = receipt::read(&receipt.path(paths))?;
    acknowledge(&mut latest);
    latest.save(paths)?;
    if latest.phase == Phase::Acknowledged {
        crash_cut("after-ack");
    }
    Ok(())
}

pub(super) fn acknowledge(receipt: &mut Receipt) {
    if receipt.phase == Phase::Committed
        && receipt
            .mappings
            .iter()
            .all(|mapping| mapping.ready || mapping.abandoned)
    {
        receipt.phase = Phase::Acknowledged;
    }
}
