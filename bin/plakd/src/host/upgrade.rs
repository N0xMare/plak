//! Same-user, instance-bound coordination for same-key host handoff.

mod bindings;
mod progress;
mod qualification;
mod reopen;
mod requester;
mod runner_carry;
mod step_drain;

use std::io::{BufRead as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plakous_hosted::host_service::wire::{AdmissionPause, SessionHost};
use serde::{Deserialize, Serialize};

use crate::carry::upgrade::{OFFLINE_MIGRATION_REQUIRED, process};
use crate::deploy::{DeployRunnerInspection, DeployRunnerVerdict};
use crate::home::paths::PlakPaths;
use crate::shell::services::Services;
use crate::surface_protocol::PlakExtension;
use crate::{CliFailure, CliResult, ProcessOutcome};

const PROBE_BOUND: Duration = Duration::from_secs(30);

#[derive(Deserialize, Serialize)]
struct Identity {
    protocol: u32,
    key: String,
    instance: String,
    binary: String,
    #[serde(default)]
    deploy_idle: Option<bool>,
}

use requester::Request;

#[derive(Deserialize)]
#[serde(untagged)]
enum Incoming {
    Upgrade(Request),
    DeployInspection(DeployRunnerInspection),
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Answer {
    Progress {
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        qualification: Option<progress::Timing>,
    },
    Ready {
        receipt: Option<uuid::Uuid>,
    },
    Abandoned {
        mappings: Vec<Abandoned>,
    },
    Refused(Refusal),
}

#[derive(Deserialize, Serialize)]
struct Abandoned {
    successor: String,
    export: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct Refusal {
    detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<RefusalReason>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RefusalReason {
    MissingDescriptorHandoff,
}

impl Refusal {
    fn offline_migration_required() -> Self {
        Self {
            detail: OFFLINE_MIGRATION_REQUIRED.into(),
            reason: None,
        }
    }

    fn missing_descriptor_handoff() -> Self {
        Self {
            detail: "host_upgrade_missing_descriptor_handoff: the destination does not support descriptor handoff; upgrade through a supported intermediate or stop the source and cold-start the destination".into(),
            reason: Some(RefusalReason::MissingDescriptorHandoff),
        }
    }
}

impl From<CliFailure> for Refusal {
    fn from(failure: CliFailure) -> Self {
        Self {
            detail: failure.message().to_owned(),
            reason: None,
        }
    }
}

pub(super) struct Ready {
    step_drain: step_drain::PreparedDrain,
    session_home: plakous_hosted::session_home::SessionHome,
    pub(super) executable: PathBuf,
    pub(super) key: String,
    pub(super) binary: String,
    image: process::VerifiedImage,
    _pause: AdmissionPause,
    _standalone: std::fs::File,
}

pub(super) type Pending = Arc<Mutex<Option<Ready>>>;
pub(super) type Worker = Arc<Mutex<Option<std::thread::JoinHandle<()>>>>;

pub(super) struct RequestControl {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    scanning: Arc<std::sync::atomic::AtomicUsize>,
}

pub(super) struct Coordinator {
    pending: Pending,
    worker: Worker,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Coordinator {
    pub(super) fn start(
        runtime: &tokio::runtime::Runtime,
        listener: tokio::net::UnixListener,
        host: &Arc<SessionHost<PlakExtension>>,
        services: &Services,
        arguments: Arc<Vec<String>>,
        scanning: Arc<std::sync::atomic::AtomicUsize>,
        inventory_ready: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Self {
        let pending: Pending = Arc::default();
        let worker: Worker = Arc::default();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let serving = serve(
            listener,
            Arc::clone(host),
            services.clone(),
            Arc::clone(&pending),
            Arc::clone(&worker),
            Arc::new(RequestControl {
                cancelled: Arc::clone(&cancelled),
                scanning,
            }),
            arguments,
        );
        let task = runtime.spawn(async move {
            if let Some(ready) = inventory_ready
                && ready.await.is_err()
            {
                return;
            }
            serving.await;
        });
        Self {
            pending,
            worker,
            cancelled,
            task,
        }
    }

    pub(super) fn pending(&self) -> Pending {
        Arc::clone(&self.pending)
    }

    pub(super) fn stop(
        self,
        runtime: &tokio::runtime::Runtime,
        paths: &PlakPaths,
    ) -> Option<Ready> {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.task.abort();
        let _ = runtime.block_on(self.task);
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
        remove(paths);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

pub(super) fn has_ready(pending: &Pending) -> bool {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

impl Ready {
    pub(super) async fn record_child_handoffs(&self, remaining: Duration) {
        use plakous_hosted::host_service::wire::HostStepDrain;
        use plakous_hosted::session_log::{SessionEvent, SessionLog};
        let prepared = &self.step_drain;
        let budget = if crate::test_hooks::upgrade_preparation_refused("child-handoff-no-budget") {
            Duration::ZERO
        } else {
            remaining.saturating_sub(HostStepDrain::ROOT_RELEASE_RESERVE)
        };
        for (tree, error) in prepared.drain.record_child_handoffs(budget).await {
            eprintln!("plakd host: child handoff not recorded for tree {tree}: {error}");
            let session = prepared.root_sessions.get(&tree).unwrap_or(&tree);
            if self.session_home.session_events_file(session).is_file()
                && let Ok(log) = SessionLog::open(&self.session_home, session)
            {
                let _ = log.append(
                    SessionEvent::new("session.handoff_unrecorded")
                        .str("node", "tree")
                        .str("tree", &tree)
                        .str("error", error),
                );
            }
        }
        crate::test_hooks::host_lifecycle::after_child_handoffs();
    }

    pub(super) fn paired_command(
        &self,
        home: &std::path::Path,
    ) -> CliResult<std::process::Command> {
        self.image.recheck(&self.executable)?;
        Ok(process::command(&self.executable, home, &self.key))
    }
}

pub(super) fn host_argv(command: &crate::command::HostCommand) -> CliResult<Vec<String>> {
    let mut arguments = vec!["host".to_owned()];
    if command.start == crate::command::HostStart::Detached {
        arguments.push("__serve-auto".into());
    }
    if let Some(web) = &command.web {
        arguments.extend(["--web".into(), web.listen.clone()]);
        if let Some(ui) = &web.ui {
            arguments.extend([
                "--ui".into(),
                ui.to_str()
                    .ok_or_else(|| CliFailure::new("host upgrade requires a UTF-8 UI path"))?
                    .to_owned(),
            ]);
        }
        if let Some(login) = &web.dev_login {
            arguments.extend(["--dev-login".into(), login.clone()]);
        }
    }
    if let Some(idle) = command.idle_release {
        arguments.extend(["--idle-release".into(), idle.as_secs().to_string()]);
    }
    Ok(arguments)
}

fn socket(paths: &PlakPaths) -> PathBuf {
    paths.home().join("host/upgrade.sock")
}

pub(super) fn request(
    services: &Services,
    expected: &str,
    drain: crate::command::HostDrain,
    inspection: Option<DeployRunnerInspection>,
) -> CliResult<ProcessOutcome> {
    if !crate::carry::upgrade::binary_id_valid(expected)
        || expected != crate::build_identity::binary_id()
    {
        return Err(CliFailure::new(
            "host_upgrade_binary_mismatch: invoke the destination build with its own --expect-binary id",
        ));
    }
    let paths = services.paths()?;
    let live = crate::wire_transport::hosts::live(paths)?;
    let [source] = live.as_slice() else {
        return Err(CliFailure::new(
            "host_upgrade_source_unavailable: exactly one running source host is required; no host was stopped",
        ));
    };
    if source.key != crate::home::state::state_key() {
        return Err(CliFailure::new(OFFLINE_MIGRATION_REQUIRED));
    }
    if let Some(inspection) = &inspection {
        requester::prove_deploy_source(paths, inspection)?;
    }
    let mut stream = crate::wire_transport::hosts::socket_until(&socket(paths), Instant::now() + PROBE_BOUND)
        .map_err(|_| CliFailure::new("host_upgrade_unsupported_source: source has no supported upgrade coordinator; retain its executable and settle it using its own lifecycle"))?;
    stream
        .set_read_timeout(
            drain
                .duration()
                .map(|duration| duration + Duration::from_secs(600)),
        )
        .map_err(io_failure)?;
    stream
        .set_write_timeout(Some(PROBE_BOUND))
        .map_err(io_failure)?;
    let identity: Identity = read(&stream)?;
    if identity.protocol != crate::carry::upgrade::PROTOCOL {
        return Err(CliFailure::new(
            "host_upgrade_unsupported_source: coordinator protocol differs; settle and drain using the retained source build before starting this destination",
        ));
    }
    if identity.key != source.key
        || !crate::carry::upgrade::binary_id_valid(&identity.binary)
        || plakous_recursive_subagents::tree::HostInstance::parse(&identity.instance).is_err()
        || inspection
            .as_ref()
            .is_some_and(|inspection| inspection.source_instance != identity.instance)
    {
        return Err(CliFailure::new(
            "host upgrade source identity changed during discovery",
        ));
    }
    if identity.binary == expected
        && identity.key == crate::home::state::state_key()
        && !crate::test_hooks::force_host_upgrade()
    {
        return Ok(ProcessOutcome::Success);
    }
    let destination =
        std::fs::canonicalize(std::env::current_exe().map_err(io_failure)?).map_err(io_failure)?;
    process::verify_image(&destination, expected)?;
    let prior_instance = identity.instance.clone();
    write(
        &mut stream,
        &Request {
            source_instance: identity.instance,
            source_key: identity.key,
            destination,
            destination_key: crate::home::state::state_key(),
            expected_binary: expected.to_owned(),
            wait_seconds: 0,
            drain,
            abandon: None,
            inspect_runners: inspection
                .map_or_else(Vec::new, |inspection| inspection.deploy_inspect_runners),
        },
    )?;
    finish_request(paths, expected, &prior_instance, &stream)
}

fn finish_request(
    paths: &PlakPaths,
    expected: &str,
    prior_instance: &str,
    stream: &UnixStream,
) -> CliResult<ProcessOutcome> {
    let mut replies = std::io::BufReader::new(stream);
    loop {
        match read_frame::<Answer>(&mut replies)? {
            Answer::Progress {
                detail,
                qualification,
            } => {
                let detail = qualification.map_or(detail, |timing| timing.detail());
                eprintln!("host upgrade: {detail}");
            }
            Answer::Abandoned { .. } | Answer::Ready { receipt: Some(_) } => {
                return Err(CliFailure::new(OFFLINE_MIGRATION_REQUIRED));
            }
            Answer::Refused(refusal) => return Err(CliFailure::new(refusal.detail)),
            Answer::Ready { receipt: None } => {
                wait_host(paths, expected, prior_instance)?;
                return Ok(ProcessOutcome::Success);
            }
        }
    }
}

fn wait_host(paths: &PlakPaths, binary: &str, prior_instance: &str) -> CliResult<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if let Ok(stream) = crate::wire_transport::hosts::socket_until(
            &socket(paths),
            Instant::now() + Duration::from_millis(100),
        ) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            if matches!(read::<Identity>(&stream), Ok(identity) if identity.key == crate::home::state::state_key() && identity.binary == binary && identity.instance != prior_instance)
            {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(CliFailure::new(
        "host_upgrade_outcome_unknown: expected host has not acknowledged readiness; if it stopped, start the destination build",
    ))
}

pub(super) fn bind(paths: &PlakPaths) -> CliResult<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = socket(paths);
    if path.try_exists().map_err(io_failure)? {
        std::fs::remove_file(&path).map_err(io_failure)?;
    }
    let listener = std::os::unix::net::UnixListener::bind(&path).map_err(io_failure)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(io_failure)?;
    listener.set_nonblocking(true).map_err(io_failure)?;
    tokio::net::UnixListener::from_std(listener).map_err(io_failure)
}

pub(super) async fn serve(
    listener: tokio::net::UnixListener,
    host: Arc<SessionHost<PlakExtension>>,
    services: Services,
    pending: Pending,
    worker: Worker,
    control: Arc<RequestControl>,
    arguments: Arc<Vec<String>>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        if !matches!(stream.peer_cred(), Ok(peer) if peer.uid() == rustix::process::geteuid().as_raw())
        {
            continue;
        }
        let Ok(stream) = stream.into_std() else {
            continue;
        };
        let Ok(()) = stream.set_nonblocking(false) else {
            continue;
        };
        // One coordinator at a time; a disconnected caller does not cancel an
        // already committed handoff.
        let host = Arc::clone(&host);
        let services = services.clone();
        let pending = Arc::clone(&pending);
        let control = Arc::clone(&control);
        let arguments = Arc::clone(&arguments);
        let runtime = tokio::runtime::Handle::current();
        let (finished, completion) = tokio::sync::oneshot::channel();
        let handle = std::thread::spawn(move || {
            let _ = handle(
                stream, &host, &services, &pending, &control, &arguments, &runtime,
            );
            let _ = finished.send(());
        });
        *worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
        let _ = completion.await;
        if let Some(handle) = worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = handle.join();
        }
    }
}

fn next_upgrade(
    stream: &mut UnixStream,
    services: &Services,
    pending: &Pending,
    runtime: &tokio::runtime::Handle,
) -> CliResult<Option<Request>> {
    stream
        .set_read_timeout(Some(PROBE_BOUND))
        .map_err(io_failure)?;
    stream
        .set_write_timeout(Some(PROBE_BOUND))
        .map_err(io_failure)?;
    let identity = plakous_recursive_subagents::tree::custody_host();
    write(
        stream,
        &Identity {
            protocol: crate::carry::upgrade::PROTOCOL,
            key: crate::home::state::state_key(),
            instance: identity.instance.to_string(),
            binary: crate::build_identity::binary_id().to_owned(),
            deploy_idle: Some(!has_ready(pending)),
        },
    )?;
    match read(stream)? {
        Incoming::Upgrade(request) => Ok(Some(request)),
        Incoming::DeployInspection(request) => {
            let verdict = runner_carry::deploy_verdict(
                &request,
                &identity.instance.to_string(),
                !has_ready(pending),
                |runners| {
                    runner_carry::inspect(
                        services,
                        runners,
                        runner_carry::Check::BeforeDrain,
                        runtime,
                    )
                },
            );
            write(stream, &verdict)?;
            Ok(None)
        }
    }
}

fn handle(
    mut stream: UnixStream,
    host: &Arc<SessionHost<PlakExtension>>,
    services: &Services,
    pending: &Pending,
    control: &RequestControl,
    arguments: &[String],
    runtime: &tokio::runtime::Handle,
) -> CliResult<()> {
    let Some(request) = next_upgrade(&mut stream, services, pending, runtime)? else {
        return Ok(());
    };
    if request.destination_key != crate::home::state::state_key()
        || request.wait_seconds != 0
        || request.abandon.is_some()
    {
        return write(
            &mut stream,
            &Answer::Refused(Refusal::offline_migration_required()),
        );
    }
    let caller = requester::Caller::watch(&stream, Arc::clone(&control.cancelled))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(io_failure)?;
    let output = Mutex::new(stream.try_clone().map_err(io_failure)?);
    let progress_start = Instant::now();
    let last_progress = Mutex::new(None::<Instant>);
    let progress = |event: progress::Event<'_>| {
        let (detail, qualification) = match event {
            progress::Event::Qualification(timing) => (timing.detail(), Some(timing)),
            progress::Event::Waiting(detail) => {
                let mut last = last_progress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !detail.starts_with("preflight connected ")
                    && last.is_some_and(|sent| sent.elapsed() < Duration::from_secs(1))
                {
                    return Ok(());
                }
                *last = Some(Instant::now());
                (
                    format!("waiting {}s: {detail}", progress_start.elapsed().as_secs()),
                    None,
                )
            }
        };
        let mut output = output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        write(
            &mut output,
            &Answer::Progress {
                detail,
                qualification,
            },
        )
    };
    let result = prepare(
        host,
        services,
        request,
        &caller.cancelled,
        arguments,
        runtime,
        &progress,
    );
    match result {
        Ok(ready) => {
            runtime.block_on(super::lifecycle::restarting(host));
            caller.ready();
            crate::carry::upgrade::crash_cut("before-answer");
            publish_ready(pending, ready, host);
            write(&mut stream, &Answer::Ready { receipt: None })
        }
        Err(refusal) => {
            resume_rollback(host, services, runtime, control);
            super::lifecycle::running(host);
            write(&mut stream, &Answer::Refused(refusal))
        }
    }
}

// The accept loop joins the prior worker before greeting. A Ready transfer
// remains owned until replacement, even after that worker exits.
fn publish_ready(pending: &Pending, ready: Ready, host: &SessionHost<PlakExtension>) {
    let mut pending = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *pending = Some(ready);
    // The observer runs under the publication lock, before any acceptor can
    // see ready. Moving the lifecycle publication after this seam is detectable.
    crate::test_hooks::host_lifecycle::record_ready_order(host.lifecycle().is_some_and(|notice| {
        notice.phase == plakd_wire_types::lifecycle::HostLifecyclePhase::Restarting
    }));
}

fn resume_rollback(
    host: &Arc<SessionHost<PlakExtension>>,
    services: &Services,
    runtime: &tokio::runtime::Handle,
    control: &RequestControl,
) {
    use plakous_hosted::host_service::wire::SlotView;
    let owed = host
        .slots()
        .into_iter()
        .filter_map(|slot| {
            let id = match slot.peek() {
                SlotView::Dormant(id) | SlotView::Opening(id) | SlotView::Releasing(id) => id,
                SlotView::Resident(session) => session.session_id(),
            };
            match super::sessions::continuation_owed(services, &id) {
                Ok(true) => Some(id),
                Ok(false) => None,
                Err(failure) => {
                    eprintln!(
                        "plakd host: cannot check rollback continuation {id}: {}",
                        failure.message()
                    );
                    None
                }
            }
        })
        .collect();
    let cancelled = Arc::clone(&control.cancelled);
    super::scan(
        host,
        owed,
        move || cancelled.load(std::sync::atomic::Ordering::Acquire),
        services,
        runtime,
        Arc::clone(&control.scanning),
    );
}

fn prepare(
    host: &Arc<SessionHost<PlakExtension>>,
    services: &Services,
    request: Request,
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
    arguments: &[String],
    runtime: &tokio::runtime::Handle,
    report: &progress::Report<'_>,
) -> Result<Ready, Refusal> {
    let progress = |detail: &str| report(progress::Event::Waiting(detail));
    let paths = services.paths()?;
    check_cancel(cancelled)?;
    let (destination, image, reopen) =
        qualification::qualify_destination(services, &request, arguments, report)?;
    let inspect_runner = runner_carry::before_drain(
        &request,
        services,
        &destination.runner_version,
        runtime,
        report,
    )?;
    let standalone = progress::timed(report, progress::Phase::StandaloneAdmission, || {
        crate::carry::upgrade::admission(paths, true)
    })?;
    let pause = host.pause_admissions_for_upgrade(Arc::clone(cancelled))?;
    super::lifecycle::draining(host, &request.expected_binary, &request.destination_key);
    let budget = step_drain::Budget::new(request.drain)?;
    progress("resolving earlier admission verdicts")?;
    host.resolve_paused_drafts(&pause, budget.barrier_deadline, cancelled)?;
    let step_drain = step_drain::prepare(
        host, services, &pause, budget, cancelled, runtime, &progress,
    )?;
    pause.commit()?;
    host.join_paused_stops(&pause, Instant::now() + Duration::from_secs(10))?;
    qualification::recheck_held(services, &request, arguments, &reopen, cancelled)?;
    if inspect_runner {
        runner_carry::held(&request, services, runtime)?;
    }
    image.recheck(&request.destination)?;
    check_cancel(cancelled)?;
    Ok(Ready {
        step_drain,
        session_home: paths.session_home(),
        executable: request.destination,
        key: request.destination_key,
        binary: request.expected_binary,
        image,
        _pause: pause,
        _standalone: standalone,
    })
}

fn check_cancel(cancelled: &std::sync::atomic::AtomicBool) -> CliResult<()> {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        Err(CliFailure::new(
            "host_upgrade_cancelled: the source is stopping; no new surrender is admitted",
        ))
    } else {
        Ok(())
    }
}

fn io_failure(error: std::io::Error) -> CliFailure {
    CliFailure::new(format!("host upgrade control channel: {error}"))
}

fn read<T: serde::de::DeserializeOwned>(stream: &UnixStream) -> CliResult<T> {
    read_frame(&mut std::io::BufReader::new(stream))
}

fn read_frame<T: serde::de::DeserializeOwned>(reader: &mut impl std::io::BufRead) -> CliResult<T> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    reader
        .take(64 * 1024 + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(io_failure)?;
    if bytes.len() > 64 * 1024 || bytes.last() != Some(&b'\n') {
        return Err(CliFailure::new(
            "host upgrade control frame exceeds its bound or is incomplete",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| CliFailure::new("host upgrade control frame is invalid"))
}

fn write(stream: &mut UnixStream, value: &impl Serialize) -> CliResult<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| CliFailure::new(error.to_string()))?;
    stream
        .write_all(&bytes)
        .and_then(|()| stream.write_all(b"\n"))
        .map_err(io_failure)
}

pub(super) fn remove(paths: &PlakPaths) {
    let _ = std::fs::remove_file(socket(paths));
}
