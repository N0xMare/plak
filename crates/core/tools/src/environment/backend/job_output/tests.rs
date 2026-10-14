use super::super::{
    ExecutionOperationControl, JoinedProcessCommand, JoinedProcessSessions,
    JoinedProcessShellProfile, JoinedProcessStartRequest, JoinedProcessTerminalMode,
};
use super::*;
use crate::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, JobStatus,
};
use plakous_model::{BranchId, CancellationToken, RunId, SessionId};
use std::os::unix::fs::PermissionsExt;
use std::str::FromStr;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const SESSION: &str = "00000000-0000-0000-0000-0000000000b0";
const OTHER_SESSION: &str = "00000000-0000-0000-0000-0000000000b1";
const BRANCH: &str = "00000000-0000-0000-0000-0000000000c0";
const OWNER: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
const OWNER_HEX: &str = "0123456789abcdef";
/// The owner digits of an earlier incarnation of the same service.
const EARLIER_HEX: &str = "fedcba9876543210";

/// A job layout file name of [`BRANCH`] for job `number` and incarnation `owner`.
fn layout(number: u32, owner: &str) -> String {
    format!("{BRANCH}-p{number}-{owner}.log")
}

/// Creates a private session directory under the jobs root.
fn session_directory(state: &Path, session: &str) -> TestResult<std::path::PathBuf> {
    let directory = state.join("jobs").join(session);
    std::fs::create_dir(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    Ok(directory)
}

type Gate = (
    String,
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);
static RELEASE_GATE: std::sync::Mutex<Option<Gate>> = std::sync::Mutex::new(None);

/// Holds a consumption release for the gated root until the test lets it go.
pub(super) fn release_gate(root: &str) {
    let gate = {
        let mut gate = RELEASE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if gate.as_ref().is_some_and(|(path, _, _)| path == root) {
            gate.take()
        } else {
            None
        }
    };
    if let Some((_, started, release)) = gate {
        let _ = started.send(());
        let _ = release.recv_timeout(Duration::from_secs(20));
    }
}

/// Consumes on a thread without a runtime, so the release runs inline.
fn consume_inline(sessions: &JoinedProcessSessions, number: u32) -> TestResult {
    let sessions = sessions.clone();
    std::thread::spawn(move || sessions.consume_jobs(&[number]))
        .join()
        .map_err(|_| "consumption panicked")?;
    Ok(())
}

fn set_modified(path: impl AsRef<Path>, to: SystemTime) -> TestResult {
    // A read-only descriptor suffices for the owner, also on a consumed 0400 file.
    File::open(path)?.set_modified(to)?;
    Ok(())
}

fn mode(path: impl AsRef<Path>) -> TestResult<u32> {
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
}

fn host_environment(workspace: &Path) -> TestResult<ExecutionEnvironment> {
    Ok(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-0000000000a0")?),
        workspace,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?)
}

fn bound(
    workspace: &Path,
    root: &JobOutputRoot,
    session: &str,
) -> TestResult<ExecutionEnvironment> {
    let environment = host_environment(workspace)?;
    environment.joined_process_sessions()?.bind_job_outputs(
        root.clone(),
        SessionId::from_str(session)?,
        BranchId::from_str(BRANCH)?,
    );
    Ok(environment)
}

fn request(command: &str) -> TestResult<JoinedProcessStartRequest> {
    Ok(JoinedProcessStartRequest::new(
        JoinedProcessCommand::shell(
            JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()])?,
            command,
        )?,
        ".",
        Vec::new(),
        true,
        JoinedProcessTerminalMode::Pipes,
    )?)
}

/// Starts `printf before; read line; printf after`, promotes it once
/// `before` is in the window, then releases it and waits for its file to close.
fn promoted_job(environment: &ExecutionEnvironment) -> TestResult<(u32, String)> {
    let sessions = environment.joined_process_sessions()?;
    let id = sessions.start(
        request("printf before; read line; printf after")?,
        ExecutionOperationControl::new(CancellationToken::new()),
    )?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while sessions
        .poll_from(
            id,
            super::super::JoinedProcessOutputCursor::new(0),
            4096,
            256,
        )?
        .records()
        .is_empty()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "no output before promotion"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    sessions.promote(id)?;
    sessions.write_stdin(
        id,
        b"go\n",
        super::super::JoinedProcessInputDisposition::Close,
    )?;
    let number = JoinedProcessSessions::job_number(id);
    while sessions
        .jobs()
        .iter()
        .any(|job| job.number == number && job.status == JobStatus::Running)
    {
        assert!(std::time::Instant::now() < deadline, "job did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    let path = sessions
        .job_output_file(number)
        .ok_or("promoted job has no issued output file")?
        .path;
    // The monitor closes the spool after the terminal join.
    while std::fs::read_to_string(&path)? != "beforeafter" {
        assert!(std::time::Instant::now() < deadline, "spool incomplete");
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok((number, path))
}

async fn read(
    environment: &ExecutionEnvironment,
    path: &str,
) -> Option<Result<JobOutputSlice, ExecutionBackendError>> {
    match environment.read_job_output(path, 0, 1024) {
        Some(future) => Some(future.await),
        None => None,
    }
}

fn refused_kind(
    result: Option<Result<JobOutputSlice, ExecutionBackendError>>,
) -> TestResult<ExecutionBackendErrorKind> {
    match result {
        Some(Err(error)) => {
            assert!(
                error.message().as_str().starts_with("job output file "),
                "{error:?}"
            );
            Ok(error.kind())
        }
        Some(Ok(slice)) => Err(format!("read was admitted: {slice:?}").into()),
        None => Err("issued path was not recognised".into()),
    }
}

#[tokio::test]
async fn promoted_job_spools_window_and_later_output_to_private_exclusive_file() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let (number, path) = promoted_job(&environment)?;
    let canonical = std::fs::canonicalize(state.path())?;
    let prefix = format!("{}/jobs/{SESSION}/{BRANCH}-p{number}-", canonical.display());
    let owner = path
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(".log"))
        .ok_or(format!("{path} is not under {prefix}"))?;
    assert_eq!(owner.len(), 16);
    assert!(owner.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(mode(canonical.join("jobs"))?, 0o700);
    assert_eq!(mode(canonical.join("jobs").join(SESSION))?, 0o700);
    assert_eq!(mode(&path)?, 0o600);
    let slice = read(&environment, &path)
        .await
        .ok_or("issued path refused")??;
    assert_eq!(slice.bytes, b"beforeafter");
    assert!(slice.complete);
    Ok(())
}

#[tokio::test]
async fn only_the_exact_issued_path_of_this_service_is_admitted() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let (number, path) = promoted_job(&environment)?;
    let sibling = path.replace(&format!("-p{number}-"), &format!("-p{}-", number + 1));
    std::fs::write(&sibling, b"sibling")?;
    // Spellings under the root's issued prefix are refused without any I/O.
    for other in [
        sibling.clone(),
        format!("{path}/"),
        path.replace("/jobs/", "/jobs/./"),
    ] {
        assert_eq!(
            refused_kind(read(&environment, &other).await)?,
            ExecutionBackendErrorKind::WorkspacePolicy,
            "{other}"
        );
    }
    // Other spellings are left to workspace containment.
    for other in [
        path.replace("/jobs/", "//jobs/"),
        path.trim_start_matches('/').to_owned(),
    ] {
        assert!(read(&environment, &other).await.is_none(), "{other}");
    }
    // Another agent (its own service) and another session never read this path.
    let other_agent = bound(workspace.path(), &root, SESSION)?;
    assert_eq!(
        refused_kind(read(&other_agent, &path).await)?,
        ExecutionBackendErrorKind::WorkspacePolicy
    );
    let other_session = bound(workspace.path(), &root, OTHER_SESSION)?;
    let (_, other_path) = promoted_job(&other_session)?;
    assert_eq!(
        refused_kind(read(&environment, &other_path).await)?,
        ExecutionBackendErrorKind::WorkspacePolicy
    );
    assert_eq!(
        refused_kind(read(&other_session, &path).await)?,
        ExecutionBackendErrorKind::WorkspacePolicy
    );
    // An unbound service issues nothing.
    assert!(
        read(&host_environment(workspace.path())?, &path)
            .await
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn swapped_or_deleted_issued_files_are_typed_refusals() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let (_, path) = promoted_job(&environment)?;
    let file = Path::new(&path);
    let session_dir = file.parent().ok_or("session dir")?;

    // A second link to the inode.
    let link = session_dir.join("link");
    std::fs::hard_link(file, &link)?;
    assert_eq!(
        refused_kind(read(&environment, &path).await)?,
        ExecutionBackendErrorKind::IdentityMismatch
    );
    std::fs::remove_file(&link)?;
    assert!(read(&environment, &path).await.ok_or("refused")?.is_ok());

    // A replaced file: the old inode stays open so its number cannot be reused.
    let original = File::open(file)?;
    let replacement = session_dir.join("replacement");
    std::fs::write(&replacement, b"forged")?;
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600))?;
    let moved = session_dir.join("moved");
    std::fs::rename(file, &moved)?;
    std::fs::rename(&replacement, file)?;
    assert_eq!(
        refused_kind(read(&environment, &path).await)?,
        ExecutionBackendErrorKind::IdentityMismatch
    );

    // A symlink at the file component, even to the original inode.
    std::fs::remove_file(file)?;
    std::os::unix::fs::symlink(&moved, file)?;
    assert_eq!(
        refused_kind(read(&environment, &path).await)?,
        ExecutionBackendErrorKind::SymlinkEscape
    );
    std::fs::remove_file(file)?;
    std::fs::rename(&moved, file)?;
    assert!(read(&environment, &path).await.ok_or("refused")?.is_ok());
    drop(original);

    // A symlink at the session-directory component.
    let real = session_dir.with_file_name("real");
    std::fs::rename(session_dir, &real)?;
    std::os::unix::fs::symlink(&real, session_dir)?;
    assert_eq!(
        refused_kind(read(&environment, &path).await)?,
        ExecutionBackendErrorKind::SymlinkEscape
    );
    std::fs::remove_file(session_dir)?;
    std::fs::rename(&real, session_dir)?;

    // Deletion is a typed not-found, for the file and for the whole session.
    std::fs::remove_file(file)?;
    assert_eq!(
        refused_kind(read(&environment, &path).await)?,
        ExecutionBackendErrorKind::NotFound
    );
    std::fs::remove_dir_all(session_dir)?;
    assert_eq!(
        refused_kind(read(&environment, &path).await)?,
        ExecutionBackendErrorKind::NotFound
    );
    Ok(())
}

#[test]
fn root_must_be_a_private_real_directory() -> TestResult {
    let state = tempfile::tempdir()?;
    let real = state.path().join("real");
    std::fs::create_dir(&real)?;
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700))?;
    let link = state.path().join("jobs");
    std::os::unix::fs::symlink(&real, &link)?;
    assert_eq!(
        JobOutputRoot::open(&link)
            .err()
            .ok_or("symlinked root opened")?
            .kind(),
        ExecutionBackendErrorKind::SymlinkEscape
    );
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o750))?;
    assert_eq!(
        JobOutputRoot::open(&real)
            .err()
            .ok_or("shared root opened")?
            .kind(),
        ExecutionBackendErrorKind::Denied
    );
    assert!(JobOutputRoot::open(Path::new("relative/jobs")).is_err());
    Ok(())
}

fn outputs(state: &Path) -> TestResult<JobOutputs> {
    Ok(JobOutputs::new(
        JobOutputRoot::open(&state.join("jobs"))?,
        SessionId::from_str(SESSION)?,
        BranchId::from_str(BRANCH)?,
        OWNER,
    ))
}

#[test]
fn creation_is_exclusive() -> TestResult {
    let state = tempfile::tempdir()?;
    let mut outputs = outputs(state.path())?;
    let directory = state.path().join("jobs").join(SESSION);
    std::fs::create_dir(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        directory.join(format!("{BRANCH}-p1-{OWNER_HEX}.log")),
        b"stale",
    )?;
    assert!(outputs.create(1).is_err());
    assert_eq!(outputs.path(1), None);
    assert!(outputs.create(2).is_ok());
    assert!(outputs.path(2).is_some());
    Ok(())
}

#[test]
fn spooling_stops_at_64_mib_with_one_truncation_line() -> TestResult {
    let state = tempfile::tempdir()?;
    let mut outputs = outputs(state.path())?;
    let mut writer = outputs.create(1)?;
    let chunk = vec![b'x'; 1024 * 1024];
    for _ in 0..65 {
        writer.append(&chunk);
    }
    writer.append(b"never written");
    drop(writer);
    let bytes = std::fs::read(outputs.path(1).ok_or("issued")?)?;
    let line = format!("\n{JOB_OUTPUT_TRUNCATED_LINE}\n");
    assert_eq!(
        u64::try_from(bytes.len())?,
        MAX_JOB_OUTPUT_FILE_BYTES + u64::try_from(line.len())?
    );
    assert!(bytes.ends_with(line.as_bytes()));
    assert_eq!(JOB_OUTPUT_TRUNCATED_LINE, "[output truncated at 64 MiB]");
    Ok(())
}

#[test]
fn retention_runs_a_day_after_consumption_and_spares_unconsumed_and_foreign_files() -> TestResult {
    let state = tempfile::tempdir()?;
    let mut outputs = outputs(state.path())?;
    for number in 1..=3 {
        outputs.create(number)?.append(b"output");
    }
    let directory = state.path().join("jobs").join(SESSION);
    let foreign = directory.join(format!("other-p1-{OWNER_HEX}.log"));
    std::fs::write(&foreign, b"another branch")?;
    std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o400))?;
    let now = SystemTime::now();
    let old = now - JOB_OUTPUT_RETENTION - Duration::from_secs(60);
    let paths = (1..=3)
        .map(|number| outputs.path(number).map(str::to_owned).ok_or("issued"))
        .collect::<Result<Vec<_>, _>>()?;
    // Every job finished long ago; only 1 and 3 are consumed now.
    for path in paths.iter().chain([&foreign.display().to_string()]) {
        set_modified(path, old)?;
    }
    let scope = outputs.scope();
    assert_eq!(
        scope.release(&[1, 3], &outputs.issued(&[1, 3]), now),
        [] as [String; 0]
    );
    assert!(paths.iter().all(|path| Path::new(path).exists()));
    assert_eq!(mode(&paths[0])?, 0o400);
    assert_eq!(mode(&paths[1])?, 0o600);
    assert_eq!(mode(&paths[2])?, 0o400);
    assert_eq!(
        scope.release(
            &[],
            &[],
            now + JOB_OUTPUT_RETENTION - Duration::from_secs(1)
        ),
        [] as [String; 0]
    );

    let swept = scope.release(&[], &[], now + JOB_OUTPUT_RETENTION);
    assert_eq!(swept.len(), 2);
    outputs.retire(&swept, &[]);
    assert!(!Path::new(&paths[0]).exists());
    assert!(Path::new(&paths[1]).exists());
    assert!(!Path::new(&paths[2]).exists());
    assert!(foreign.exists());
    assert_eq!(outputs.file(1), None);
    assert!(outputs.file(2).is_some());
    // A path this service deleted is a typed not-found, not a foreign path.
    let Some(JobOutputAdmission::Refused(error)) = outputs.admit(&paths[0]) else {
        return Err("a retired path was not refused".into());
    };
    assert_eq!(error.kind(), ExecutionBackendErrorKind::NotFound);
    assert_eq!(
        error.message().as_str(),
        format!("job output file {} no longer exists", paths[0])
    );
    Ok(())
}

#[tokio::test]
async fn consuming_a_notice_never_deletes_the_file_it_just_named() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let (number, path) = promoted_job(&environment)?;
    // The job became terminal over a day ago and its notice is only now
    // rendered (it names `path`) and consumed in the same driver step.
    set_modified(
        &path,
        SystemTime::now() - JOB_OUTPUT_RETENTION - Duration::from_secs(60),
    )?;
    let sessions = environment.joined_process_sessions()?;
    assert_eq!(
        sessions.job_output_file(number).map(|file| file.path),
        Some(path.clone())
    );
    consume_inline(&sessions, number)?;
    assert!(
        Path::new(&path).exists(),
        "the notice just named {path}, but consuming that notice deleted it"
    );
    assert_eq!(mode(&path)?, 0o400);
    assert!(read(&environment, &path).await.ok_or("refused")?.is_ok());
    Ok(())
}

#[tokio::test]
async fn consumption_release_runs_off_the_calling_task_and_outside_the_outputs_lock() -> TestResult
{
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let sessions = environment.joined_process_sessions()?;
    let (old, old_path) = promoted_job(&environment)?;
    let (fresh, fresh_path) = promoted_job(&environment)?;
    consume_inline(&sessions, old)?;
    set_modified(
        &old_path,
        SystemTime::now() - JOB_OUTPUT_RETENTION - Duration::from_secs(60),
    )?;
    let (started, started_signal) = std::sync::mpsc::sync_channel(1);
    let (release, release_signal) = std::sync::mpsc::sync_channel(1);
    *RELEASE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((root.path().to_owned(), started, release_signal));
    let begun = std::time::Instant::now();
    sessions.consume_jobs(&[fresh]);
    assert!(
        begun.elapsed() < Duration::from_secs(10),
        "consume_jobs waited for its release"
    );
    started_signal.recv_timeout(Duration::from_secs(20))?;
    // The release is parked on another thread: the outputs lock is free and
    // nothing has been deleted yet.
    assert_eq!(
        sessions.job_output_file(old).map(|file| file.path),
        Some(old_path.clone())
    );
    assert!(Path::new(&old_path).exists());
    release.send(())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while sessions.job_output_file(old).is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "release never retired"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!Path::new(&old_path).exists());
    assert!(Path::new(&fresh_path).exists());
    assert_eq!(mode(&fresh_path)?, 0o400);
    assert_eq!(
        refused_kind(read(&environment, &old_path).await)?,
        ExecutionBackendErrorKind::NotFound
    );
    Ok(())
}

#[tokio::test]
async fn a_restarted_service_refuses_earlier_issued_paths_specifically() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let before = bound(workspace.path(), &root, SESSION)?;
    let (number, path) = promoted_job(&before)?;
    // A restart or /reload: a fresh service for the same session and branch,
    // seeded past the restored job numbers. History still shows `path`.
    let after = bound(workspace.path(), &root, SESSION)?;
    after
        .joined_process_sessions()?
        .seed_job_counter(number + 1);
    assert!(Path::new(&path).exists());
    let Some(Err(error)) = read(&after, &path).await else {
        return Err("an earlier issued path was not refused".into());
    };
    assert_eq!(error.kind(), ExecutionBackendErrorKind::WorkspacePolicy);
    assert_eq!(
        error.message().as_str(),
        format!(
            "job output file {path} was issued by an earlier process or another agent and \
             cannot be read with this tool"
        )
    );
    Ok(())
}

#[tokio::test]
async fn a_restarted_service_with_a_lagging_job_counter_still_spools() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let before = bound(workspace.path(), &root, SESSION)?;
    let (number, first) = promoted_job(&before)?;
    // Crash after promotion, before the checkpoint that persists next_id: the
    // restarted service reuses the number under its own incarnation name.
    let after = bound(workspace.path(), &root, SESSION)?;
    let (reused, second) = promoted_job(&after).map_err(|error| {
        format!("restarted service promoted job p{number} again without an output file: {error}")
    })?;
    assert_eq!(reused, number);
    assert_ne!(first, second);
    assert!(Path::new(&first).exists());
    Ok(())
}

/// Creates a sparse layout file of `size` bytes in `directory`, `age` seconds
/// old, consumed (0400) or not (0600).
fn sized(
    directory: &Path,
    number: u32,
    size: u64,
    consumed: bool,
    age: Duration,
) -> TestResult<std::path::PathBuf> {
    let path = directory.join(layout(number, OWNER_HEX));
    let file = File::create(&path)?;
    file.set_len(size)?;
    file.set_modified(SystemTime::now() - age)?;
    let mode = if consumed { 0o400 } else { 0o600 };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
    Ok(path)
}

#[test]
fn root_budget_evicts_consumed_files_oldest_first_else_skips_spooling() -> TestResult {
    let state = tempfile::tempdir()?;
    let outputs = outputs(state.path())?;
    let other = session_directory(state.path(), OTHER_SESSION)?;
    let now = SystemTime::now();
    let hour = Duration::from_secs(3600);
    // Sparse files: the budget counts file sizes.
    let bound = MAX_JOB_OUTPUT_FILE_BYTES;
    let filler = sized(
        &other,
        1,
        JOB_OUTPUT_ROOT_BUDGET_BYTES - 3 * bound,
        false,
        Duration::ZERO,
    )?;
    let oldest = sized(&other, 2, bound, true, 2 * hour)?;
    let older = sized(&other, 3, bound, true, hour)?;
    let scope = outputs.scope();
    let (_, evicted, fits) = scope.prepare_create(now);
    assert!(fits && evicted.is_empty());

    let newest = sized(&other, 4, bound, true, Duration::from_secs(60))?;
    let (_, evicted, fits) = scope.prepare_create(now);
    assert!(fits);
    assert_eq!(
        evicted,
        [format!(
            "{}/{OTHER_SESSION}/{}",
            scope.root.path(),
            layout(2, OWNER_HEX)
        )]
    );
    assert!(!oldest.exists() && older.exists() && newest.exists());

    // Unconsumed files are never evicted, and when they alone leave no room
    // nothing is evicted at all: spooling is skipped instead.
    File::options()
        .write(true)
        .open(&filler)?
        .set_len(JOB_OUTPUT_ROOT_BUDGET_BYTES)?;
    let (_, evicted, fits) = scope.prepare_create(now);
    assert!(!fits);
    assert!(evicted.is_empty(), "{evicted:?}");
    assert!(filler.exists() && older.exists() && newest.exists());
    Ok(())
}

#[test]
fn budget_eviction_keeps_consumed_files_when_eviction_cannot_make_room() -> TestResult {
    let state = tempfile::tempdir()?;
    let outputs = outputs(state.path())?;
    let other = session_directory(state.path(), OTHER_SESSION)?;
    // Unconsumed files alone already leave no room for one more file bound.
    File::create(other.join(layout(1, OWNER_HEX)))?.set_len(JOB_OUTPUT_ROOT_BUDGET_BYTES)?;
    // A file reported an hour ago, well inside its 24 h retention.
    let reported = other.join(layout(2, OWNER_HEX));
    std::fs::write(&reported, b"reported output")?;
    set_modified(&reported, SystemTime::now() - Duration::from_secs(3600))?;
    std::fs::set_permissions(&reported, std::fs::Permissions::from_mode(0o400))?;
    let (_, evicted, fits) = outputs.scope().prepare_create(SystemTime::now());
    assert!(!fits);
    assert!(
        evicted.is_empty() && reported.exists(),
        "evicted {evicted:?} although no eviction could make room for the new file"
    );
    Ok(())
}

#[test]
fn budget_evicts_stale_unconsumed_files_after_consumed_ones() -> TestResult {
    let state = tempfile::tempdir()?;
    let outputs = outputs(state.path())?;
    let other = session_directory(state.path(), OTHER_SESSION)?;
    let bound = MAX_JOB_OUTPUT_FILE_BYTES;
    let week = JOB_OUTPUT_STALE_AGE + Duration::from_secs(60);
    let fresh = sized(
        &other,
        1,
        JOB_OUTPUT_ROOT_BUDGET_BYTES - 2 * bound,
        false,
        Duration::ZERO,
    )?;
    let consumed = sized(&other, 2, bound, true, Duration::from_secs(60))?;
    let stalest = sized(&other, 3, bound, false, week + Duration::from_secs(60))?;
    let week_old = sized(&other, 4, bound, false, week)?;
    let scope = outputs.scope();
    let (_, evicted, fits) = scope.prepare_create(SystemTime::now());
    assert!(fits);
    let path = |number| {
        format!(
            "{}/{OTHER_SESSION}/{}",
            scope.root.path(),
            layout(number, OWNER_HEX)
        )
    };
    assert_eq!(evicted, [path(2), path(3)]);
    assert!(fresh.exists() && !consumed.exists() && !stalest.exists() && week_old.exists());
    Ok(())
}

#[test]
fn sweeps_and_the_budget_ignore_names_outside_the_job_layout() -> TestResult {
    let state = tempfile::tempdir()?;
    let outputs = outputs(state.path())?;
    let other = session_directory(state.path(), OTHER_SESSION)?;
    let foreign = session_directory(state.path(), "not-a-session")?;
    let old = JOB_OUTPUT_STALE_AGE + Duration::from_secs(60);
    let uppercase = format!("{}-p1-{OWNER_HEX}.log", BRANCH.to_uppercase());
    let mut kept = Vec::new();
    for name in [
        "x-p1.log".to_owned(),
        uppercase,
        format!("{BRANCH}-p01-{OWNER_HEX}.log"),
        format!("{BRANCH}-p1-{}.log", OWNER_HEX.to_uppercase()),
        format!("{BRANCH}-p1-{OWNER_HEX}.txt"),
    ] {
        let path = other.join(name);
        File::create(&path)?.set_len(JOB_OUTPUT_ROOT_BUDGET_BYTES)?;
        set_modified(&path, SystemTime::now() - old)?;
        kept.push(path);
    }
    let path = foreign.join(layout(1, OWNER_HEX));
    File::create(&path)?.set_len(JOB_OUTPUT_ROOT_BUDGET_BYTES)?;
    set_modified(&path, SystemTime::now() - old)?;
    kept.push(path);
    // None of them counts toward the budget or is ever deleted.
    let (_, evicted, fits) = outputs.scope().prepare_create(SystemTime::now());
    assert!(fits && evicted.is_empty());
    outputs.scope.root.sweep_stale(SystemTime::now());
    for path in &kept {
        assert!(path.exists(), "{} was deleted", path.display());
    }
    Ok(())
}

#[tokio::test]
async fn promotion_without_budget_room_succeeds_without_an_output_file() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let other = session_directory(state.path(), OTHER_SESSION)?;
    File::create(other.join(layout(1, OWNER_HEX)))?.set_len(JOB_OUTPUT_ROOT_BUDGET_BYTES)?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let sessions = environment.joined_process_sessions()?;
    let id = sessions.start(
        request("read line")?,
        ExecutionOperationControl::new(CancellationToken::new()),
    )?;
    sessions.promote(id)?;
    assert_eq!(
        sessions.job_output_file(JoinedProcessSessions::job_number(id)),
        None
    );
    sessions.write_stdin(
        id,
        b"go\n",
        super::super::JoinedProcessInputDisposition::Close,
    )?;
    Ok(())
}

#[test]
fn a_failed_spool_write_is_recorded_and_reported_by_reads() -> TestResult {
    let state = tempfile::tempdir()?;
    let mut outputs = outputs(state.path())?;
    let mut writer = outputs.create(1)?;
    writer.append(b"kept\n");
    let path = outputs.path(1).ok_or("issued")?.to_owned();
    assert_eq!(outputs.file(1).ok_or("issued")?.write_failed_after, None);
    // A read-only descriptor: every later write fails.
    writer.file = File::open(&path)?;
    writer.append(b"lost");
    writer.append(b"never attempted");
    assert_eq!(outputs.file(1).ok_or("issued")?.write_failed_after, Some(5));
    let Some(JobOutputAdmission::Issued(location)) = outputs.admit(&path) else {
        return Err("issued path refused".into());
    };
    let slice = location.read(0, 1024)?;
    assert_eq!(slice.bytes, b"kept\n");
    assert_eq!(slice.write_failed_after, Some(5));
    assert_eq!(
        job_output_incomplete_line(5),
        "[output file incomplete: write failed after 5 bytes]"
    );
    Ok(())
}

#[test]
fn consumption_marking_never_changes_a_relinked_inode() -> TestResult {
    let state = tempfile::tempdir()?;
    let outputs = outputs(state.path())?;
    let directory = session_directory(state.path(), SESSION)?;
    // A user file linked into the session directory under job file names of
    // this incarnation and of an earlier one.
    let precious = state.path().join("precious");
    std::fs::write(&precious, b"user file")?;
    std::fs::set_permissions(&precious, std::fs::Permissions::from_mode(0o644))?;
    let modified = std::fs::metadata(&precious)?.modified()?;
    std::fs::hard_link(&precious, directory.join(layout(7, OWNER_HEX)))?;
    std::fs::hard_link(&precious, directory.join(layout(7, EARLIER_HEX)))?;
    let _ = outputs.scope().release(
        &[7],
        &outputs.issued(&[7]),
        SystemTime::now() + Duration::from_secs(3600),
    );
    assert_eq!(
        mode(&precious)?,
        0o644,
        "marking chmodded an inode with another link"
    );
    assert_eq!(std::fs::metadata(&precious)?.modified()?, modified);
    Ok(())
}

#[test]
fn consumption_marks_earlier_incarnations_but_only_the_issued_inode_of_this_one() -> TestResult {
    let state = tempfile::tempdir()?;
    let mut outputs = outputs(state.path())?;
    // The issued inode stays open, so its number cannot be reused.
    let _writer = outputs.create(1)?;
    let directory = state.path().join("jobs").join(SESSION);
    // A file an earlier incarnation spooled for the same job number.
    let earlier = directory.join(layout(1, EARLIER_HEX));
    std::fs::write(&earlier, b"earlier")?;
    std::fs::set_permissions(&earlier, std::fs::Permissions::from_mode(0o600))?;
    // This incarnation's issued file, replaced under its name.
    let issued = directory.join(layout(1, OWNER_HEX));
    std::fs::remove_file(&issued)?;
    std::fs::write(&issued, b"replacement")?;
    std::fs::set_permissions(&issued, std::fs::Permissions::from_mode(0o600))?;
    let now = SystemTime::now() + Duration::from_secs(3600);
    let _ = outputs.scope().release(&[1], &outputs.issued(&[1]), now);
    assert_eq!(mode(&earlier)?, 0o400);
    assert!(std::fs::metadata(&earlier)?.modified()? > SystemTime::now());
    assert_eq!(mode(&issued)?, 0o600, "marked a replaced issued file");
    Ok(())
}

#[test]
fn a_partial_spool_write_counts_its_accepted_prefix() -> TestResult {
    let state = tempfile::tempdir()?;
    let mut outputs = outputs(state.path())?;
    let mut writer = outputs.create(1)?;
    writer.append(b"kept\n");
    // A full non-blocking pipe accepts a prefix of a large write, then refuses.
    let (mut reader, pipe) = std::io::pipe()?;
    let pipe = std::os::fd::OwnedFd::from(pipe);
    rustix::fs::fcntl_setfl(&pipe, rustix::fs::fcntl_getfl(&pipe)? | OFlags::NONBLOCK)?;
    rustix::fs::fcntl_setfl(
        &reader,
        rustix::fs::fcntl_getfl(&reader)? | OFlags::NONBLOCK,
    )?;
    writer.file = File::from(pipe);
    writer.append(&vec![b'x'; 4 * 1024 * 1024]);
    let mut accepted = Vec::new();
    match std::io::Read::read_to_end(&mut reader, &mut accepted) {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        other => return Err(format!("pipe drained unexpectedly: {other:?}").into()),
    }
    // Content the pipe accepted, before any incomplete-file line.
    let prefix = accepted.iter().take_while(|byte| **byte == b'x').count();
    assert!(prefix > 0, "the pipe accepted no prefix");
    assert_eq!(
        outputs.file(1).ok_or("issued")?.write_failed_after,
        Some(5 + u64::try_from(prefix)?)
    );
    Ok(())
}

#[test]
fn reads_stop_at_the_content_bound_plus_its_truncation_line() -> TestResult {
    let state = tempfile::tempdir()?;
    let mut outputs = outputs(state.path())?;
    drop(outputs.create(1)?);
    let path = outputs.path(1).ok_or("issued")?.to_owned();
    let bound = job_output_read_bound();
    let truncation = format!("\n{JOB_OUTPUT_TRUNCATED_LINE}\n");
    assert!(bound >= MAX_JOB_OUTPUT_FILE_BYTES + u64::try_from(truncation.len())?);
    let Some(JobOutputAdmission::Issued(location)) = outputs.admit(&path) else {
        return Err("issued path refused".into());
    };
    // Exactly at the bound the file is complete.
    File::options().write(true).open(&path)?.set_len(bound)?;
    let slice = location.read(bound - 10, 1024)?;
    assert_eq!((slice.bytes.len(), slice.complete), (10, true));
    // Beyond it nothing more is read and the file is never complete.
    File::options()
        .write(true)
        .open(&path)?
        .set_len(bound + 4096)?;
    let slice = location.read(bound - 10, 1024)?;
    assert_eq!((slice.bytes.len(), slice.complete), (10, false));
    let slice = location.read(bound, 1024)?;
    assert_eq!((slice.bytes.len(), slice.complete), (0, false));
    Ok(())
}

/// A promotion the service refuses (here: foreground cancellation won) runs
/// no sweep and evicts nothing for a job that never gets a file.
#[tokio::test]
async fn a_refused_promotion_evicts_nothing() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let other = session_directory(state.path(), OTHER_SESSION)?;
    let cap = MAX_JOB_OUTPUT_FILE_BYTES;
    sized(
        &other,
        1,
        JOB_OUTPUT_ROOT_BUDGET_BYTES - 2 * cap,
        false,
        Duration::ZERO,
    )?;
    let consumed = sized(&other, 2, cap, true, Duration::from_secs(60))?;
    sized(&other, 3, cap, false, Duration::ZERO)?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let sessions = environment.joined_process_sessions()?;
    let token = CancellationToken::new();
    let id = sessions.start(
        request("read line")?,
        ExecutionOperationControl::new(token.clone()),
    )?;
    token.cancel();
    let refused = sessions.promote(id);
    assert!(refused.is_err(), "a cancelled foreground was promoted");
    assert!(
        consumed.exists(),
        "a refused promotion ({refused:?}) evicted another session's reported file"
    );
    Ok(())
}

/// At the unconsumed-row limit a promotion is refused before any root walk,
/// so a start held there and retrying neither sweeps nor evicts.
#[tokio::test]
async fn a_promotion_refused_at_the_row_limit_sweeps_and_evicts_nothing() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let other = session_directory(state.path(), OTHER_SESSION)?;
    let cap = MAX_JOB_OUTPUT_FILE_BYTES;
    sized(
        &other,
        1,
        JOB_OUTPUT_ROOT_BUDGET_BYTES - cap,
        false,
        Duration::ZERO,
    )?;
    let evictable = sized(&other, 2, cap, true, Duration::from_secs(60))?;
    let own = session_directory(state.path(), SESSION)?;
    let swept = own.join(layout(200, EARLIER_HEX));
    std::fs::write(&swept, b"reported long ago")?;
    set_modified(
        &swept,
        SystemTime::now() - Duration::from_secs(2 * 24 * 3600),
    )?;
    std::fs::set_permissions(&swept, std::fs::Permissions::from_mode(CONSUMED_MODE))?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let sessions = environment.joined_process_sessions()?;
    let started_at = plakous_model::Timestamp::new(plakous_model::now_unix_secs(), 0)?;
    let rows = (1..=128)
        .map(|number| crate::JobView {
            number,
            command: String::from("true"),
            started_at,
            owner: String::from("root"),
            elapsed: Duration::ZERO,
            status: JobStatus::Exited(Some(0)),
            output: String::new(),
            session_id: [0; 16],
            owner_generation: 1,
        })
        .collect::<Vec<_>>();
    sessions.seed_job_counter(129);
    sessions.restore_job_terminals(&rows);
    let id = sessions.start(
        request("read line")?,
        ExecutionOperationControl::new(CancellationToken::new()),
    )?;
    for _ in 0..2 {
        let refused = sessions
            .promote(id)
            .err()
            .ok_or("a promotion beyond the row limit succeeded")?;
        assert_eq!(refused.kind(), ExecutionBackendErrorKind::ResourceLimit);
    }
    assert!(
        swept.exists(),
        "a refused promotion ran the retention sweep"
    );
    assert!(evictable.exists(), "a refused promotion evicted a file");
    sessions.write_stdin(
        id,
        b"go\n",
        super::super::JoinedProcessInputDisposition::Close,
    )?;
    Ok(())
}

/// Reservations taken before either promoter creates its file each count
/// only files that exist, so each may admit one file: concurrent promoters
/// exceed the budget by at most one file bound each, never more.
#[tokio::test]
async fn concurrent_reservations_overshoot_the_root_budget_by_at_most_one_bound_each() -> TestResult
{
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let other = session_directory(state.path(), OTHER_SESSION)?;
    let cap = MAX_JOB_OUTPUT_FILE_BYTES;
    let used = JOB_OUTPUT_ROOT_BUDGET_BYTES - cap;
    sized(&other, 1, used, false, Duration::ZERO)?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let sessions = environment.joined_process_sessions()?;
    let control = || ExecutionOperationControl::new(CancellationToken::new());
    let first = sessions.start(request("read line")?, control())?;
    let second = sessions.start(request("read line")?, control())?;
    let first_fits = sessions.reserve_job_output(first);
    let second_fits = sessions.reserve_job_output(second);
    sessions.promote_reserved(first, first_fits)?;
    sessions.promote_reserved(second, second_fits)?;
    let files = [first, second]
        .into_iter()
        .filter(|id| {
            sessions
                .job_output_file(JoinedProcessSessions::job_number(*id))
                .is_some()
        })
        .count();
    for id in [first, second] {
        sessions.write_stdin(
            id,
            b"go\n",
            super::super::JoinedProcessInputDisposition::Close,
        )?;
    }
    // Both saw room for exactly one more file; each created at most its own.
    assert!(first_fits && second_fits);
    assert_eq!(files, 2);
    let bound = used + u64::try_from(files)? * cap;
    assert_eq!(bound, JOB_OUTPUT_ROOT_BUDGET_BYTES + cap);
    Ok(())
}

/// A promotion the host's notice ledger has no room for (the refusal the
/// async start retries every 250 ms while preempted) is refused before any
/// root walk, so it sweeps and evicts nothing.
#[tokio::test]
async fn a_promotion_refused_by_a_full_notice_ledger_evicts_nothing() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let other = session_directory(state.path(), OTHER_SESSION)?;
    let cap = MAX_JOB_OUTPUT_FILE_BYTES;
    sized(
        &other,
        1,
        JOB_OUTPUT_ROOT_BUDGET_BYTES - cap,
        false,
        Duration::ZERO,
    )?;
    let evictable = sized(&other, 2, cap, true, Duration::from_secs(60))?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let sessions = environment.joined_process_sessions()?;
    sessions.observe_jobs(|_| false, || false);
    let id = sessions.start(
        request("read line")?,
        ExecutionOperationControl::new(CancellationToken::new()),
    )?;
    let refused = sessions
        .promote(id)
        .err()
        .ok_or("a promotion the ledger refused succeeded")?;
    assert_eq!(refused.kind(), ExecutionBackendErrorKind::ResourceLimit);
    let evicted = !evictable.exists();
    sessions.write_stdin(
        id,
        b"go\n",
        super::super::JoinedProcessInputDisposition::Close,
    )?;
    assert!(
        !evicted,
        "a promotion refused by a full notice ledger evicted another session's file"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn monitor_threads() -> TestResult<usize> {
    let mut count = 0;
    for task in std::fs::read_dir("/proc/self/task")? {
        let comm = std::fs::read_to_string(task?.path().join("comm")).unwrap_or_default();
        if comm.trim_end() == "plak-job-monito" {
            count += 1;
        }
    }
    Ok(count)
}

/// A ledger refusal that races the room pre-check comes after the monitor
/// started and drops its gate: the monitor thread ends without touching the
/// session, which stays an ordinary foreground process that runs to
/// completion and joins.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_refused_promotion_ends_its_monitor_and_leaves_the_foreground_untouched() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let environment = bound(workspace.path(), &root, SESSION)?;
    let sessions = environment.joined_process_sessions()?;
    let accept = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let observer = Arc::clone(&accept);
    // The room pre-check always passes, so only the authoritative record
    // refuses, after the monitor started.
    sessions.observe_jobs(
        move |_| observer.load(std::sync::atomic::Ordering::SeqCst),
        || true,
    );
    let control = || ExecutionOperationControl::new(CancellationToken::new());
    // Control: a published running job keeps exactly one waiting monitor.
    let job = sessions.start(request("read line")?, control())?;
    sessions.promote(job)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    // The spawned thread names itself, so `/proc` can show the name only
    // after `promote` has returned.
    while monitor_threads()? != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "control job has no monitor"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    accept.store(false, std::sync::atomic::Ordering::SeqCst);
    let foreground = sessions.start(request("read line; echo got-$line")?, control())?;
    let refused = sessions
        .promote(foreground)
        .err()
        .ok_or("a promotion the ledger refused succeeded")?;
    assert_eq!(refused.kind(), ExecutionBackendErrorKind::ResourceLimit);
    while monitor_threads()? != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "a refused promotion leaked its monitor thread"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        sessions.jobs().len(),
        1,
        "the refused promotion is listed as a job"
    );
    sessions.write_stdin(
        foreground,
        b"x\n",
        super::super::JoinedProcessInputDisposition::Close,
    )?;
    while sessions.joined_terminal_status(foreground).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the refused foreground never exited"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = sessions
        .poll_from(
            foreground,
            super::super::JoinedProcessOutputCursor::new(0),
            4096,
            256,
        )?
        .records()
        .iter()
        .flat_map(|record| record.bytes().iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(String::from_utf8_lossy(&output), "got-x\n");
    sessions.join(foreground)?;
    assert_eq!(monitor_threads()?, 1);
    sessions.write_stdin(
        job,
        b"go\n",
        super::super::JoinedProcessInputDisposition::Close,
    )?;
    Ok(())
}
