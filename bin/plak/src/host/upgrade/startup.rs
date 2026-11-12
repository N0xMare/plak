//! Host-owned imports acknowledge each successor only after fresh reconciliation.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
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
    let services = runner::custody(services)?;
    for receipt in pending(&services)? {
        crash_cut("before-import");
        for mapping in receipt.mappings.iter().filter(|mapping| !mapping.ready) {
            match importing::local(&services, &receipt, mapping) {
                Ok(true) => {
                    if let Err(failure) = importing::one(&services, &receipt, mapping) {
                        deferred(&mapping.successor, &failure);
                    }
                }
                Ok(false) => {}
                Err(failure) => deferred(&mapping.successor, &failure),
            }
        }
    }
    runner::prepare(&services, None)
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

fn deferred(successor: &str, failure: &CliFailure) {
    eprintln!(
        "host upgrade successor {successor} remains withheld; destination recovery will retry: {}",
        crate::cli_output::safe_terminal_block(failure.message())
    );
}

pub(in crate::host) struct Recovery {
    stop: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Recovery {
    pub(in crate::host) fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.send(());
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
) -> Recovery {
    let (wake, events) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::clone(&stop);
    let services = services.clone();
    let host = Arc::clone(host);
    let runtime = runtime.clone();
    let worker = std::thread::spawn(move || {
        while !cancelled.load(Ordering::Acquire) {
            let receipts = match pending(&services) {
                Ok(receipts) if receipts.is_empty() => break,
                Ok(receipts) => receipts,
                Err(failure) => {
                    deferred("receipt", &failure);
                    break;
                }
            };
            for mut receipt in receipts {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                if let Err(failure) = advance(&services, &runtime, &mut receipt, &cancelled, &host)
                {
                    deferred("receipt", &failure);
                }
                host.changed();
            }
            if events.recv_timeout(Duration::from_secs(2)).is_ok() {
                break;
            }
        }
    });
    Recovery {
        stop,
        wake,
        worker: Some(worker),
    }
}

fn advance(
    services: &Services,
    runtime: &tokio::runtime::Handle,
    receipt: &mut Receipt,
    stop: &AtomicBool,
    host: &plakous_hosted::host_service::wire::SessionHost<crate::surface_protocol::PlakExtension>,
) -> CliResult<()> {
    let mut order = (0..receipt.mappings.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| {
        !importing::local(services, receipt, &receipt.mappings[*index]).unwrap_or(false)
    });
    for index in order {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if receipt.mappings[index].ready {
            continue;
        }
        let id = match importing::one(services, receipt, &receipt.mappings[index]) {
            Ok(id) => id,
            Err(failure) => {
                deferred(&receipt.mappings[index].successor, &failure);
                continue;
            }
        };
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
        receipt.mappings[index].ready = true;
        receipt.save(services.paths()?)?;
        host.changed();
    }
    if receipt.mappings.iter().all(|mapping| mapping.ready) {
        receipt.phase = Phase::Acknowledged;
        receipt.save(services.paths()?)?;
    }
    if receipt.phase == Phase::Acknowledged {
        crash_cut("after-ack");
    }
    Ok(())
}
