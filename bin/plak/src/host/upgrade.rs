//! Same-user, instance-bound upgrade coordination; the export is the only data carrier.

mod abandon;
mod bindings;
mod preparation;
mod startup;
pub(super) use abandon::request as abandon;
pub(super) use startup::{prepare_host, start_retry};

use std::io::{BufRead as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plakous_hosted::host_service::wire::{AdmissionPause, SessionHost};
use serde::{Deserialize, Serialize};

use crate::carry::upgrade::{
    DestinationProbe, process,
    receipt::{self, Phase, Receipt},
};
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
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    source_instance: String,
    source_key: String,
    destination: PathBuf,
    destination_key: String,
    expected_binary: String,
    wait_seconds: u64,
    abandon: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Answer {
    Ready { receipt: Option<uuid::Uuid> },
    Abandoned { mappings: Vec<abandon::Abandoned> },
    Refused { detail: String },
}

#[derive(Serialize)]
struct Blocker {
    session: String,
    kind: BlockerKind,
    detail: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum BlockerKind {
    RunningTurn,
    LiveChild,
    DaemonJobs,
    HeldWork,
    UnsettledInput,
    UnboundRoot,
    Unavailable,
}

impl From<&crate::carry::ExportRefusal> for BlockerKind {
    fn from(refusal: &crate::carry::ExportRefusal) -> Self {
        use crate::carry::ExportRefusal;
        match refusal {
            ExportRefusal::NoRuntime => Self::UnboundRoot,
            ExportRefusal::UnsettledIntent => Self::UnsettledInput,
            ExportRefusal::SubagentRunning { .. } => Self::LiveChild,
            ExportRefusal::TurnUnfinished => Self::RunningTurn,
            ExportRefusal::BackgroundProcesses => Self::DaemonJobs,
            ExportRefusal::TreeUnreadable(_)
            | ExportRefusal::RecordBehindHead
            | ExportRefusal::Runtime(_)
            | ExportRefusal::ProfileMissing => Self::Unavailable,
        }
    }
}

pub(super) struct Ready {
    pub(super) executable: PathBuf,
    pub(super) key: String,
    pub(super) binary: String,
    leases: Leases,
    pause: AdmissionPause,
    _standalone: std::fs::File,
}

pub(super) type Pending = Arc<Mutex<Option<Ready>>>;
pub(super) type Worker = Arc<Mutex<Option<std::thread::JoinHandle<()>>>>;

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
    ) -> Self {
        let pending: Pending = Arc::default();
        let worker: Worker = Arc::default();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = runtime.spawn(serve(
            listener,
            Arc::clone(host),
            services.clone(),
            Arc::clone(&pending),
            Arc::clone(&worker),
            Arc::clone(&cancelled),
            arguments,
        ));
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

pub(super) fn replace(
    ready: Option<Ready>,
    services: &Services,
    command: &crate::command::HostCommand,
    stop: crate::web::StopProbe,
    arguments: &[String],
    home_lock: super::lock::HostLock,
) -> CliResult<ProcessOutcome> {
    use std::os::unix::process::CommandExt as _;
    let Some(ready) = ready.filter(|_| !stop.stopped()) else {
        return Ok(ProcessOutcome::Success);
    };
    process::verify_image(&ready.executable, &ready.binary)?;
    let mut replacement = process::command(&ready.executable, services.paths()?.home(), &ready.key);
    replacement.args(arguments);
    let same_key = ready.key == crate::home::state::state_key();
    drop(ready);
    let failure = replacement.exec();
    drop(home_lock);
    if same_key {
        eprintln!("host upgrade executable could not start: {failure}; restoring the old host");
        return super::serve_host(services, command);
    }
    Err(CliFailure::new(format!(
        "host upgrade committed but destination could not start: {failure}; retry using its recorded executable"
    )))
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

pub(super) struct StartupReplacement {
    pub(super) executable: PathBuf,
    pub(super) key: String,
    pub(super) binary: String,
    pub(super) argv: Vec<String>,
}

impl StartupReplacement {
    pub(super) fn exec(self, paths: &PlakPaths) -> CliResult<ProcessOutcome> {
        use std::os::unix::process::CommandExt as _;
        process::verify_image(&self.executable, &self.binary)?;
        let failure = process::command(&self.executable, paths.home(), &self.key)
            .args(&self.argv)
            .exec();
        Err(CliFailure::new(format!(
            "host upgrade owner recovery could not start: {failure}"
        )))
    }
}

/// Called only under the home guard: a prepared transfer's previous host
/// is gone, so it is withdrawn; a committed source never becomes owner again.
pub(super) fn startup_replacement(paths: &PlakPaths) -> CliResult<Option<StartupReplacement>> {
    let own = crate::home::state::state_key();
    let mut receipts = receipt::all(paths)?;
    for receipt in &mut receipts {
        if receipt.phase == Phase::Aborted
            && receipt.source_key == own
            && receipt.source_archival_pending
        {
            preparation::withdraw(paths, receipt)?;
        }
    }
    let Some(mut receipt) = receipts.pop() else {
        return Ok(None);
    };
    let unresolved = matches!(receipt.phase, Phase::Prepared | Phase::Committed);
    if receipt.phase == Phase::Aborted {
        if receipt.source_key == own && receipt.source_archival_pending {
            preparation::withdraw(paths, &mut receipt)?;
        }
        return Ok(None);
    }
    if !unresolved && receipt.source_key != own && receipt.destination_key != own {
        return Ok(None);
    }
    if receipt.phase == Phase::Prepared {
        if receipt.source_key == own {
            preparation::withdraw(paths, &mut receipt)?;
            return Ok(None);
        }
        if process::verify_image(&receipt.source_executable, &receipt.source_binary).is_err() {
            receipt.phase = Phase::Aborted;
            receipt.save(paths)?;
            return Ok(None);
        }
        return Ok(Some(StartupReplacement {
            executable: receipt.source_executable,
            key: receipt.source_key,
            binary: receipt.source_binary,
            argv: receipt.host_argv,
        }));
    }
    let (executable, key, binary) = if receipt.phase == Phase::Aborted {
        (
            receipt.source_executable,
            receipt.source_key,
            receipt.source_binary,
        )
    } else {
        (
            receipt.destination,
            receipt.destination_key,
            receipt.destination_binary,
        )
    };
    if key == own && (!unresolved || binary == crate::build_identity::binary_id()) {
        return Ok(None);
    }
    process::verify_image(&executable, &binary)?;
    Ok(Some(StartupReplacement {
        executable,
        key,
        binary,
        argv: receipt.host_argv,
    }))
}

fn socket(paths: &PlakPaths) -> PathBuf {
    paths.home().join("host/upgrade.sock")
}

pub(super) fn request(
    services: &Services,
    expected: &str,
    wait: Duration,
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
    if live.is_empty() {
        return recover(paths, expected);
    }
    let [source] = live.as_slice() else {
        return Err(CliFailure::new(
            "host_upgrade_source_unavailable: exactly one running source host is required; no host was stopped",
        ));
    };
    let mut stream = crate::wire_transport::hosts::socket_until(&socket(paths), Instant::now() + PROBE_BOUND)
        .map_err(|_| CliFailure::new("host_upgrade_unsupported_source: source has no supported upgrade coordinator; retain its executable and settle it using its own lifecycle"))?;
    stream
        .set_read_timeout(Some(wait + Duration::from_secs(600)))
        .map_err(io_failure)?;
    stream
        .set_write_timeout(Some(PROBE_BOUND))
        .map_err(io_failure)?;
    let identity: Identity = read(&stream)?;
    if identity.protocol != crate::carry::upgrade::PROTOCOL
        || identity.key != source.key
        || !crate::carry::upgrade::binary_id_valid(&identity.binary)
        || plakous_recursive_subagents::tree::HostInstance::parse(&identity.instance).is_err()
    {
        return Err(CliFailure::new(
            "host upgrade source identity changed during discovery",
        ));
    }
    if identity.binary == expected && identity.key == crate::home::state::state_key() {
        if let Some(receipt) = receipt::all(paths)?.into_iter().rev().find(|receipt| {
            receipt.destination_binary == expected
                && receipt.destination_key == identity.key
                && matches!(receipt.phase, Phase::Committed | Phase::Acknowledged)
        }) {
            wait_ack(paths, receipt.id)?;
            print_mappings(&receipt)?;
        }
        return Ok(ProcessOutcome::Success);
    }
    let destination =
        std::fs::canonicalize(std::env::current_exe().map_err(io_failure)?).map_err(io_failure)?;
    process::verify_image(&destination, expected)?;
    write(
        &mut stream,
        &Request {
            source_instance: identity.instance,
            source_key: identity.key,
            destination,
            destination_key: crate::home::state::state_key(),
            expected_binary: expected.to_owned(),
            wait_seconds: wait.as_secs(),
            abandon: None,
        },
    )?;
    match read::<Answer>(&stream)? {
        Answer::Abandoned { .. } => Err(CliFailure::new("unexpected abandon reply to upgrade")),
        Answer::Refused { detail } => Err(CliFailure::new(detail)),
        Answer::Ready { receipt } => {
            if let Some(id) = receipt {
                wait_ack(paths, id)?;
                let record = receipt::read(
                    &receipt::root(paths)
                        .join(id.to_string())
                        .join("receipt.json"),
                )?;
                print_mappings(&record)?;
            } else {
                wait_host(paths, &crate::home::state::state_key(), expected)?;
            }
            Ok(ProcessOutcome::Success)
        }
    }
}

fn print_mappings(receipt: &Receipt) -> CliResult<()> {
    let mut stdout = std::io::stdout().lock();
    for mapping in &receipt.mappings {
        if mapping.abandoned {
            writeln!(stdout, "{} -> abandoned (reserved successor {}; import a copy of the retained export onto another placement)", mapping.source, mapping.successor)
                .map_err(|_| CliFailure::new("stdout failed"))?;
            continue;
        }
        writeln!(
            stdout,
            "{} -> {} ({})",
            mapping.source, mapping.successor, receipt.destination_key
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(())
}

fn wait_ack(paths: &PlakPaths, id: uuid::Uuid) -> CliResult<()> {
    let path = receipt::root(paths)
        .join(id.to_string())
        .join("receipt.json");
    let mut deadline = Instant::now() + Duration::from_secs(90);
    let mut missing_since = None;
    let mut completed = 0;
    loop {
        let receipt = receipt::read(&path)?;
        if receipt.phase == Phase::Acknowledged {
            return Ok(());
        }
        if receipt.phase == Phase::Aborted {
            return Err(CliFailure::new(
                "host_upgrade_aborted: preparation was withdrawn without surrender; recover the source key before retrying",
            ));
        }
        let progress = receipt
            .mappings
            .iter()
            .filter(|mapping| mapping.ready)
            .count();
        if progress > completed {
            completed = progress;
            deadline = Instant::now() + Duration::from_secs(90);
        }
        if home_is_held(paths)? {
            missing_since = None;
            if !coordinator_is_ready(paths) {
                deadline = Instant::now() + Duration::from_secs(90);
            }
        } else if missing_since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(3)
        {
            return Err(CliFailure::new(
                "host_upgrade_outcome_unknown: no host holds the home; retry the recorded destination to recover its receipts",
            ));
        }
        if Instant::now() >= deadline {
            return Err(CliFailure::new(format!(
                "host_upgrade_outcome_unknown: retry the same destination upgrade to recover receipt {id}; do not reopen the source"
            )));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn home_is_held(paths: &PlakPaths) -> CliResult<bool> {
    crate::wire_transport::hosts::lock_holder(&crate::wire_transport::hosts::home_lock(paths))
        .map(|holder| holder.is_some())
}

fn coordinator_is_ready(paths: &PlakPaths) -> bool {
    let Ok(stream) = UnixStream::connect(socket(paths)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    read::<Identity>(&stream).is_ok()
}

fn wait_host(paths: &PlakPaths, key: &str, binary: &str) -> CliResult<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if let Ok(stream) = crate::wire_transport::hosts::socket_until(
            &socket(paths),
            Instant::now() + Duration::from_millis(100),
        ) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            if matches!(read::<Identity>(&stream), Ok(identity) if identity.key == key && identity.binary == binary)
            {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(CliFailure::new(
        "host_upgrade_outcome_unknown: replacement has not acknowledged readiness; retry the upgrade using its recorded executable",
    ))
}

fn recover(paths: &PlakPaths, expected: &str) -> CliResult<ProcessOutcome> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::process::CommandExt as _;
    let receipt = receipt::all(paths)?.pop().ok_or_else(|| {
        CliFailure::new("host_upgrade_source_unavailable: no running source or recoverable receipt")
    })?;
    if receipt.destination_binary != expected
        || receipt.destination_key != crate::home::state::state_key()
    {
        return Err(CliFailure::new(
            "host upgrade recovery belongs to another destination; use the receipt's exact executable",
        ));
    }
    let _admission = crate::carry::upgrade::admission(paths, true)?;
    let home_lock =
        super::lock::HostLock::take(&crate::wire_transport::hosts::home_lock(paths), expected)?;
    if !crate::wire_transport::hosts::live(paths)?.is_empty() {
        return Err(CliFailure::new(
            "host started during upgrade recovery; retry against its identity",
        ));
    }
    let restore_source = matches!(receipt.phase, Phase::Prepared | Phase::Aborted);
    let (executable, key, binary) = if restore_source {
        (
            &receipt.source_executable,
            &receipt.source_key,
            &receipt.source_binary,
        )
    } else {
        (
            &receipt.destination,
            &receipt.destination_key,
            &receipt.destination_binary,
        )
    };
    process::verify_image(executable, binary)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(receipt.path(paths).with_file_name("recovery.log"))
        .map_err(io_failure)?;
    let mut child = process::command(executable, paths.home(), key)
        .args(&receipt.host_argv)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(io_failure)?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    drop(home_lock);
    wait_host(paths, key, binary)?;
    if restore_source {
        Err(CliFailure::new(
            "host_upgrade_aborted: the interrupted preparation was withdrawn and the old host is usable; retry the upgrade",
        ))
    } else {
        wait_ack(paths, receipt.id)?;
        print_mappings(&receipt)?;
        Ok(ProcessOutcome::Success)
    }
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
    cancelled: Arc<std::sync::atomic::AtomicBool>,
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
        // already committed transfer. Subsequent callers use the receipt.
        let host = Arc::clone(&host);
        let services = services.clone();
        let pending = Arc::clone(&pending);
        let cancel = Arc::clone(&cancelled);
        let arguments = Arc::clone(&arguments);
        let (finished, completion) = tokio::sync::oneshot::channel();
        let handle = std::thread::spawn(move || {
            let _ = handle(stream, &host, &services, &pending, &cancel, &arguments);
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

fn handle(
    mut stream: UnixStream,
    host: &Arc<SessionHost<PlakExtension>>,
    services: &Services,
    pending: &Pending,
    cancelled: &std::sync::atomic::AtomicBool,
    arguments: &[String],
) -> CliResult<()> {
    stream
        .set_read_timeout(Some(PROBE_BOUND))
        .map_err(io_failure)?;
    stream
        .set_write_timeout(Some(PROBE_BOUND))
        .map_err(io_failure)?;
    let identity = plakous_recursive_subagents::tree::custody_host();
    write(
        &mut stream,
        &Identity {
            protocol: crate::carry::upgrade::PROTOCOL,
            key: crate::home::state::state_key(),
            instance: identity.instance.to_string(),
            binary: crate::build_identity::binary_id().to_owned(),
        },
    )?;
    let request: Request = read(&stream)?;
    if request.abandon.is_some() {
        let answer = match abandon::apply(services, &request) {
            Ok(mappings) => {
                host.changed();
                Answer::Abandoned { mappings }
            }
            Err(failure) => Answer::Refused {
                detail: failure.message().to_owned(),
            },
        };
        return write(&mut stream, &answer);
    }
    let result = prepare(host, services, request, cancelled, arguments);
    match result {
        Ok((ready, receipt)) => {
            crate::carry::upgrade::crash_cut("before-answer");
            *pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ready);
            write(&mut stream, &Answer::Ready { receipt })
        }
        Err(failure) => write(
            &mut stream,
            &Answer::Refused {
                detail: failure.message().to_owned(),
            },
        ),
    }
}

fn prepare(
    host: &Arc<SessionHost<PlakExtension>>,
    services: &Services,
    request: Request,
    cancelled: &std::sync::atomic::AtomicBool,
    arguments: &[String],
) -> CliResult<(Ready, Option<uuid::Uuid>)> {
    let paths = services.paths()?;
    check_cancel(cancelled)?;
    if receipt::all(paths)?
        .iter()
        .any(|receipt| receipt.phase == Phase::Committed)
    {
        return Err(CliFailure::new(
            "host_upgrade_pending_ack: finish the recorded destination's pending successors before another upgrade",
        ));
    }
    qualify_destination(services, &request, arguments)?;
    let cross_key = request.source_key != request.destination_key;
    if cross_key {
        match settled_roots(services) {
            Ok(roots) => preflight_capacities(services, &request, &roots)?,
            Err(error) if request.wait_seconds == 0 => return Err(error),
            Err(_) => {}
        }
    }
    let standalone = crate::carry::upgrade::admission(paths, true)?;
    let pause = host.pause_admissions()?;
    let deadline = Instant::now() + Duration::from_secs(request.wait_seconds);
    if cross_key {
        loop {
            check_cancel(cancelled)?;
            match settled_roots(services) {
                Ok(roots) => {
                    preflight_capacities(services, &request, &roots)?;
                    break;
                }
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
    let ready = Ready {
        executable: request.destination.clone(),
        key: request.destination_key.clone(),
        binary: request.expected_binary.clone(),
        pause,
        leases: Leases::new()?,
        _standalone: standalone,
    };
    if !cross_key {
        return Ok((ready, None));
    }
    let failures = host.hand_off_paused(
        &ready.pause,
        super::HANDOFF_DEADLINE.saturating_sub(super::STOP_MARGIN),
    )?;
    if !failures.is_empty() {
        return Err(CliFailure::new(
            "host_upgrade_drain_failed: source residents did not finish their planned release; no source surrendered",
        ));
    }
    // An intent accepted immediately before the pause may have reached its
    // mailbox after the first check. The joined release persists it first.
    let documents = settled_roots(services)?;
    carry_settled(services, request, ready, documents, cancelled, arguments)
}

fn qualify_destination(
    services: &Services,
    request: &Request,
    arguments: &[String],
) -> CliResult<()> {
    let paths = services.paths()?;
    let identity = plakous_recursive_subagents::tree::custody_host();
    if request.source_instance != identity.instance.to_string()
        || request.source_key != crate::home::state::state_key()
        || !request.destination.is_absolute()
        || !crate::carry::upgrade::state_key_valid(&request.destination_key)
        || request.wait_seconds > 3600
    {
        return Err(CliFailure::new(
            "host_upgrade_stale_source: source instance or state key changed",
        ));
    }
    process::verify_image(
        &std::env::current_exe().map_err(io_failure)?,
        crate::build_identity::binary_id(),
    )?;
    process::verify_image(&request.destination, &request.expected_binary)?;
    let probe_directory = tempfile::tempdir_in(paths.home()).map_err(io_failure)?;
    let service_file = probe_directory.path().join("service.json");
    let probe_arguments = arguments
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| {
            (!(index == 1 && argument == "__serve-auto")).then_some(argument)
        })
        .collect::<Vec<_>>();
    std::fs::write(
        &service_file,
        serde_json::to_vec(&crate::carry::upgrade::ServiceProbe {
            arguments: probe_arguments.into_iter().cloned().collect(),
            placements: probe_placements(paths)?,
        })
        .map_err(|error| CliFailure::new(error.to_string()))?,
    )
    .map_err(io_failure)?;
    let mut probe_command =
        process::command(&request.destination, paths.home(), &request.destination_key);
    probe_command
        .args(["version", "--host-upgrade-probe", "--host-upgrade-service"])
        .arg(service_file);
    let probe: DestinationProbe = serde_json::from_slice(
        &process::run(probe_command, PROBE_BOUND)?.stdout,
    )
    .map_err(|_| CliFailure::new("host upgrade destination did not answer its capability probe"))?;
    if probe.protocol != crate::carry::upgrade::PROTOCOL
        || probe.receipt_version != receipt::VERSION
        || !probe.host_available
        || probe.binary_id != request.expected_binary
        || probe.state_key != request.destination_key
        || probe.config_error.is_some()
        || probe.state_key != request.source_key
            && !probe.export_versions.contains(&crate::carry::VERSION)
    {
        return Err(CliFailure::new(format!(
            "host_upgrade_destination_unsupported: {}",
            probe
                .config_error
                .as_deref()
                .unwrap_or("identity, import version or host capability does not match")
        )));
    }
    if request.source_key == request.destination_key {
        if probe.runner_protocol != crate::commands::runner::version().protocol {
            return Err(CliFailure::new(
                "host_upgrade_runner_protocol: settle and drain the old build's daemon before changing runner protocols",
            ));
        }
        preflight_same_key(services, request)?;
    }
    Ok(())
}

fn new_receipt(services: &Services, request: Request, arguments: &[String]) -> CliResult<Receipt> {
    let paths = services.paths()?;
    let identity = plakous_recursive_subagents::tree::custody_host();
    Ok(Receipt {
        version: receipt::VERSION,
        id: uuid::Uuid::new_v4(),
        sequence: receipt::all(paths)?
            .last()
            .map_or(Some(1), |receipt| receipt.sequence.checked_add(1))
            .ok_or_else(|| CliFailure::new("host upgrade receipt sequence exhausted"))?,
        source_key: request.source_key,
        source_instance: identity.instance.to_string(),
        source_binary: crate::build_identity::binary_id().to_owned(),
        source_executable: std::env::current_exe().map_err(io_failure)?,
        host_argv: arguments.to_vec(),
        destination_key: request.destination_key,
        destination_binary: request.expected_binary,
        destination: request.destination,
        phase: Phase::Prepared,
        source_archival_pending: false,
        mappings: Vec::new(),
    })
}

fn carry_settled(
    services: &Services,
    request: Request,
    ready: Ready,
    documents: Vec<crate::store::sessions::SessionRecord>,
    cancelled: &std::sync::atomic::AtomicBool,
    arguments: &[String],
) -> CliResult<(Ready, Option<uuid::Uuid>)> {
    let paths = services.paths()?;
    let mut receipt = new_receipt(services, request, arguments)?;
    let directory = receipt
        .path(paths)
        .parent()
        .ok_or_else(|| CliFailure::new("upgrade has no directory"))?
        .to_path_buf();
    for directory in [
        paths.home().join("host"),
        receipt::root(paths),
        directory.clone(),
    ] {
        crate::commands::runner::private_directory(&directory)?;
    }
    process::verify_image(&receipt.source_executable, &receipt.source_binary)?;
    if let Err(failure) = receipt.save(paths) {
        let _ = preparation::withdraw(paths, &mut receipt);
        return Err(failure);
    }
    crate::carry::upgrade::crash_cut("after-receipt");
    if let Err(failure) = preparation::exports(services, &ready, &mut receipt, documents, cancelled)
    {
        if let Err(cleanup) = preparation::withdraw(paths, &mut receipt) {
            eprintln!(
                "host upgrade preparation cleanup needs recovery: {}",
                cleanup.message()
            );
        }
        return Err(failure);
    }
    receipt.phase = Phase::Committed;
    receipt.source_archival_pending = false;
    // From this point a failed fsync is outcome-unknown, never an abort.
    if let Err(failure) = receipt.save(paths) {
        eprintln!("host upgrade commit needs recovery: {}", failure.message());
    }
    crate::carry::upgrade::crash_cut("after-commit");
    Ok((ready, Some(receipt.id)))
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

fn probe_placements(paths: &PlakPaths) -> CliResult<Vec<crate::carry::upgrade::PlacementProbe>> {
    let withheld = receipt::withheld_targets(paths)?;
    let mut seen = std::collections::BTreeSet::new();
    let records =
        crate::store::ProductStore::inspect_upgrade_roots(paths).map_err(CliFailure::new)?;
    Ok(records
        .into_iter()
        .filter(|record| !record.archived && !withheld.contains(&record.id))
        .filter_map(|record| {
            let runner = record.runner?;
            seen.insert((runner.runner_id.clone(), record.workspace_root.clone()))
                .then_some(crate::carry::upgrade::PlacementProbe {
                    id: runner.runner_id,
                    workspace: record.workspace_root,
                })
        })
        .collect())
}

fn preflight_capacities(
    services: &Services,
    request: &Request,
    roots: &[crate::store::sessions::SessionRecord],
) -> CliResult<()> {
    let paths = services.paths()?;
    let directory = tempfile::tempdir_in(paths.home()).map_err(io_failure)?;
    for record in roots {
        let file = directory.path().join(format!("{}.json", record.id));
        let document = crate::carry::export_session(&source_paths(paths, record), record)
            .map_err(|error| CliFailure::new(format!("host_upgrade_blocked: {error}")))?;
        crate::carry::write_document(&file, &document).map_err(CliFailure::new)?;
        let mut command =
            process::command(&request.destination, paths.home(), &request.destination_key);
        command
            .args(["version", "--host-upgrade-capacity"])
            .arg(file);
        process::run(command, PROBE_BOUND)?;
    }
    Ok(())
}

fn preflight_same_key(services: &Services, request: &Request) -> CliResult<()> {
    let paths = services.paths()?;
    bindings::preflight(paths, &request.destination, &request.destination_key)?;
    for record in
        crate::store::ProductStore::inspect_upgrade_roots(paths).map_err(CliFailure::new)?
    {
        if record.plakous_locator.is_none() {
            continue;
        }
        let mut command =
            process::command(&request.destination, paths.home(), &request.destination_key);
        command
            .args(["version", "--host-upgrade-reopen", "--reopen-probe"])
            .arg(&record.id);
        let verdict: crate::carry::ReopenProbe =
            serde_json::from_slice(&process::run(command, PROBE_BOUND)?.stdout).map_err(|_| {
                CliFailure::new(
                    "same-key upgrade destination did not answer its exact-reopen probe",
                )
            })?;
        match verdict {
            crate::carry::ReopenProbe::Opens => {}
            crate::carry::ReopenProbe::Unknown { reason }
                if reason == plakous_hosted::runtime::REOPEN_UNSETTLED_WORK => {}
            verdict => {
                return Err(CliFailure::new(format!(
                    "host_upgrade_reopen_refused: {}: {verdict:?}; same-key upgrade uses planned handoff, not carry",
                    record.id
                )));
            }
        }
    }
    Ok(())
}

fn source_paths(paths: &PlakPaths, record: &crate::store::sessions::SessionRecord) -> PlakPaths {
    if record.runner.is_some() {
        paths.for_remote_workspace(&record.workspace_root)
    } else {
        paths.for_workspace(&record.workspace_root)
    }
}

fn settled_roots(services: &Services) -> CliResult<Vec<crate::store::sessions::SessionRecord>> {
    let paths = services.paths()?;
    let roots =
        crate::store::ProductStore::inspect_upgrade_roots(paths).map_err(CliFailure::new)?;
    let withheld = receipt::withheld_targets(paths)?;
    if roots
        .iter()
        .filter(|record| !withheld.contains(&record.id))
        .count()
        > 4096
    {
        return Err(CliFailure::new(
            "too many carried roots for one host upgrade",
        ));
    }
    let mut documents = Vec::new();
    let mut blockers = Vec::new();
    for record in roots {
        if withheld.contains(&record.id) {
            continue;
        }
        receipt::ensure_open(paths, &record)?;
        let standing = plakous_hosted::runtime::root_standing(
            services.session_records()?.as_ref(),
            &record.id,
        )
        .map_err(CliFailure::new)?;
        if matches!(
            standing,
            plakous_hosted::runtime::RootStanding::Continuing { .. }
        ) {
            blockers.push(Blocker {
                session: record.id.clone(),
                kind: BlockerKind::HeldWork,
                detail: "a root continuation remains held or pending".into(),
            });
            continue;
        }
        if held_topology(paths, &record)? {
            blockers.push(Blocker {
                session: record.id.clone(),
                kind: BlockerKind::HeldWork,
                detail: "a child continuation or workspace operation is not settled".into(),
            });
            continue;
        }
        match crate::carry::export_session(&source_paths(paths, &record), &record) {
            Ok(_) => documents.push(record),
            Err(refusal) => blockers.push(Blocker {
                session: record.id.clone(),
                kind: BlockerKind::from(&refusal),
                detail: refusal.to_string(),
            }),
        }
    }
    if blockers.is_empty() {
        Ok(documents)
    } else {
        Err(CliFailure::new(format!(
            "host_upgrade_blocked: {}",
            serde_json::to_string(&blockers).map_err(|error| CliFailure::new(error.to_string()))?
        )))
    }
}

fn held_topology(
    paths: &PlakPaths,
    record: &crate::store::sessions::SessionRecord,
) -> CliResult<bool> {
    let (_, observed) = crate::topology_product::inspect_session_tree(paths, record)?;
    let plakous_recursive_subagents_store::TreeInspection::Tree(tree) = observed else {
        return Ok(false);
    };
    let rows = tree.rows();
    Ok(rows.nodes().values().any(|node| {
        node.parent_agent_id.is_some() && rows.loss_continuation(&node.agent_id).is_some()
    }) || rows
        .workspaces()
        .values()
        .any(|workspace| workspace.status.is_in_flight()))
}

struct Leases {
    sender: std::sync::mpsc::Sender<Option<plakous::sessions::BranchHold>>,
    worker: Option<std::thread::JoinHandle<()>>,
    valid: Arc<std::sync::atomic::AtomicBool>,
}

impl Leases {
    fn new() -> CliResult<Self> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let valid = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let health = Arc::clone(&valid);
        let executor = plakous::application::ApplicationExecutor::new()
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let worker = std::thread::spawn(move || {
            let mut held: Vec<plakous::sessions::BranchHold> = Vec::new();
            loop {
                match receiver.recv_timeout(Duration::from_secs(1)) {
                    Ok(Some(lease)) => held.push(lease),
                    Ok(None) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                for lease in &mut held {
                    if !matches!(executor.block_on(lease.renew()), Ok(Ok(()))) {
                        health.store(false, std::sync::atomic::Ordering::Release);
                    }
                }
            }
            for lease in held {
                let _ = executor.block_on(lease.release());
            }
        });
        Ok(Self {
            sender,
            worker: Some(worker),
            valid,
        })
    }

    fn hold(
        &self,
        services: &Services,
        record: &crate::store::sessions::SessionRecord,
    ) -> CliResult<()> {
        let sessions = services.state_roots()?.runtime_sessions()?;
        let executor = plakous::application::ApplicationExecutor::new()
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let locator = record
            .plakous_locator
            .ok_or_else(|| CliFailure::new("host upgrade source has no runtime"))?;
        let operation =
            plakous::sessions::SessionOperationId::new(format!("plak:host-upgrade:{}", record.id))
                .map_err(|error| CliFailure::new(error.to_string()))?;
        let lease = executor
            .block_on(sessions.hold_branch(&operation, locator))
            .map_err(|error| CliFailure::new(error.to_string()))?
            .map_err(|error| CliFailure::new(error.to_string()))?;
        self.sender
            .send(Some(lease))
            .map_err(|_| CliFailure::new("host upgrade lease keeper ended"))
    }

    fn check(&self) -> CliResult<()> {
        if self.valid.load(std::sync::atomic::Ordering::Acquire) {
            Ok(())
        } else {
            Err(CliFailure::new(
                "host upgrade source lease was lost before commit",
            ))
        }
    }
}

impl Drop for Leases {
    fn drop(&mut self) {
        let _ = self.sender.send(None);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn io_failure(error: std::io::Error) -> CliFailure {
    CliFailure::new(format!("host upgrade control channel: {error}"))
}

fn read<T: serde::de::DeserializeOwned>(stream: &UnixStream) -> CliResult<T> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::io::BufReader::new(stream)
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
