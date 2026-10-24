use super::*;
use plakous::model::{AgentId, BranchId, CancellationToken, RunId, SessionId};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    ExecutionOperationControl, JobStatus, JobStopReason, JoinedProcessCommand,
    JoinedProcessShellProfile, JoinedProcessStartRequest, JoinedProcessTerminalMode,
};
use std::{path::Path, str::FromStr, time::Duration};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn attributed_host_looks_up_only_the_exact_session_branch() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    let owner = owner("00000000-0000-0000-0000-000000000702")?;
    let host = registry.for_agent("root", "s");
    let service = host.attach(owner, sessions(root.path())?)?;
    let id = start(&service)?;
    let found = host
        .service(owner.session_id, owner.branch_id)
        .ok_or("missing service")?;
    assert!(found.retains_session(id));
    let foreign = BranchId::from_str("00000000-0000-0000-0000-000000000705")?;
    assert!(host.service(owner.session_id, foreign).is_none());
    service.stop(id, JobStopReason::User)?;
    Ok(())
}

#[test]
fn recent_terminal_rows_stay_with_their_source_branch() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    let first = owner("00000000-0000-0000-0000-000000000702")?;
    let second = owner("00000000-0000-0000-0000-000000000705")?;
    let host = registry.for_agent("root", "s");
    let service = host.attach(first, sessions(root.path())?)?;
    let id = start(&service)?;
    plakous_tools::internal::promote(&service, id)?;
    service.stop(id, JobStopReason::User)?;
    host.observe_terminal(first, &service.jobs());
    assert_eq!(registry.snapshot().len(), 1);
    host.attach(second, sessions(root.path())?)?;
    assert_eq!(registry.snapshot(), [] as [ProcessView; 0]);
    host.attach(first, sessions(root.path())?)?;
    assert_eq!(registry.snapshot().len(), 1);
    Ok(())
}

#[test]
fn parked_child_jobs_remain_visible_with_owner_and_hud_count() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    let root_owner = owner("00000000-0000-0000-0000-000000000702")?;
    registry
        .for_agent("root", "s")
        .attach(root_owner, sessions(root.path())?)?;
    let child_owner = owner("00000000-0000-0000-0000-000000000705")?;
    let host = registry.for_agent("c7", "s");
    let service = host.attach(child_owner, sessions(root.path())?)?;
    plakous_tools::internal::set_job_owner(&service, child_owner.agent_id.to_string());
    let id = start(&service)?;
    plakous_tools::internal::promote(&service, id)?;
    plakous_tools::internal::close_job_admission(&service, &child_owner.agent_id.to_string());
    drop(host);
    assert_eq!(registry.running_count(), 1);
    let rows = registry.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, "c7");
    assert_eq!(rows[0].status, ProcessStatus::Running);
    service.stop(id, JobStopReason::AttemptEnded)?;
    assert_eq!(registry.running_count(), 0);
    assert_eq!(registry.snapshot()[0].owner, "c7");
    assert_eq!(registry.snapshot()[0].status, ProcessStatus::Stopped);
    Ok(())
}

/// A child that attaches before the root is still that root's slice once
/// the root is recorded. Ending the job drops the running count and keeps
/// the child owner on the recent row.
#[test]
fn child_attached_before_root_remains_in_the_idle_snapshot() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    let child_owner = owner("00000000-0000-0000-0000-000000000708")?;
    let service = registry
        .for_agent("c1", "s")
        .attach(child_owner, sessions(root.path())?)?;
    let id = start(&service)?;
    plakous_tools::internal::promote(&service, id)?;
    assert_eq!(registry.snapshot()[0].owner, "c1");
    registry.for_agent("root", "s").attach(
        owner("00000000-0000-0000-0000-000000000702")?,
        sessions(root.path())?,
    )?;
    let rows = registry.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, "c1");
    assert_eq!(rows[0].status, ProcessStatus::Running);
    assert_eq!(registry.running_count(), 1);
    service.stop(id, JobStopReason::AttemptEnded)?;
    assert_eq!(registry.running_count(), 0);
    assert_eq!(registry.snapshot()[0].owner, "c1");
    assert_eq!(registry.snapshot()[0].status, ProcessStatus::Stopped);
    Ok(())
}

/// A same-session fork keeps the child and drops the previous root branch.
/// Switching to another session leaves the child with its source.
#[test]
fn same_session_root_fork_keeps_the_child_and_drops_the_old_root_slice() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    let root_host = registry.for_agent("root", "s");
    let first = root_host.attach(
        owner("00000000-0000-0000-0000-000000000702")?,
        sessions(root.path())?,
    )?;
    let root_job = start(&first)?;
    plakous_tools::internal::promote(&first, root_job)?;
    let child = registry.for_agent("c1", "s").attach(
        owner("00000000-0000-0000-0000-000000000708")?,
        sessions(root.path())?,
    )?;
    let child_job = start(&child)?;
    plakous_tools::internal::promote(&child, child_job)?;
    root_host.attach(
        owner("00000000-0000-0000-0000-000000000705")?,
        sessions(root.path())?,
    )?;
    let rows = registry.snapshot();
    assert_eq!(
        rows.len(),
        1,
        "the previous root slice leaves; the child stays"
    );
    assert_eq!(rows[0].owner, "c1");
    assert_eq!(rows[0].status, ProcessStatus::Running);
    let other = JobOwner {
        session_id: SessionId::from_str("00000000-0000-0000-0000-000000000709")?,
        branch_id: BranchId::from_str("00000000-0000-0000-0000-00000000070a")?,
        agent_id: AgentId::from_str("00000000-0000-0000-0000-000000000703")?,
    };
    registry
        .for_agent("root", "other")
        .attach(other, sessions(root.path())?)?;
    assert!(
        registry.snapshot().iter().all(|row| row.owner != "c1"),
        "another session's root does not take this child"
    );
    child.stop(child_job, JobStopReason::AttemptEnded)?;
    first.stop(root_job, JobStopReason::SessionExit)?;
    Ok(())
}

#[test]
fn recent_terminal_hook_captures_before_any_product_poll() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    let owner = owner("00000000-0000-0000-0000-000000000702")?;
    let host = registry.for_agent("c3", "s");
    let service = host.attach(owner, sessions(root.path())?)?;
    let id = start(&service)?;
    plakous_tools::internal::promote(&service, id)?;
    service.stop(id, JobStopReason::User)?;
    host.observe_terminal(owner, &service.jobs());
    plakous_tools::internal::consume_jobs(&service, &[JoinedProcessSessions::job_number(id)]);
    let rows = registry.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, "c3");
    assert_eq!(rows[0].status, ProcessStatus::Stopped);
    Ok(())
}

/// A job that retires between the number lookup and the stop has
/// nothing left to stop: that is a stopped job, not a stop failure.
#[test]
fn a_job_retired_before_its_user_stop_is_not_a_failure() -> TestResult {
    let root = tempfile::tempdir()?;
    let service = sessions(root.path())?;
    let id = start(&service)?;
    plakous_tools::internal::promote(&service, id)?;
    let number = JoinedProcessSessions::job_number(id);
    service.stop(id, JobStopReason::User)?;
    plakous_tools::internal::consume_jobs(&service, &[number]);
    assert!(service.session_for_number(number).is_none());
    let stopped = service.stop_with_grace(id, JobStopReason::User, USER_STOP_GRACE);
    assert!(stopped.is_err());
    settle_numbered_stop(stopped, service.session_for_number(number) != Some(id))?;
    Ok(())
}

#[test]
fn processes_stop_exact_identity_and_stop_all_are_user_attributed() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let first = registry.attach(
        owner("00000000-0000-0000-0000-000000000702")?,
        sessions(root.path())?,
    )?;
    let second = registry.attach(
        owner("00000000-0000-0000-0000-000000000705")?,
        sessions(root.path())?,
    )?;
    for service in [&first, &second] {
        let id = start(service)?;
        plakous_tools::internal::promote(service, id)?;
    }
    let rows = registry.snapshot();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].id, rows[1].id);
    registry.stop_user(Some(&rows[0].id))?;
    registry.settle_user_stops();
    assert_eq!(registry.running_count(), 1);
    assert!(registry.stop_user(Some("foreign:p1")).is_err());
    assert_eq!(registry.running_count(), 1);
    registry.stop_user(None)?;
    registry.settle_user_stops();
    assert_eq!(registry.running_count(), 0);
    assert_eq!(registry.take_user_stop_failures(), [] as [String; 0]);
    assert!(
        first
            .jobs()
            .iter()
            .chain(second.jobs().iter())
            .all(|job| job.status == JobStatus::Stopped(JobStopReason::User))
    );
    Ok(())
}

/// A user stop returns at once with the row `Stopping`, and the process
/// gets the TaskStop grace between SIGTERM and SIGKILL. The TERM handler
/// here finishes only after the test releases it, well past any short
/// cleanup bound, so a SIGKILL inside the grace leaves no marker.
#[test]
fn user_stop_grants_the_term_grace_without_blocking_the_caller() -> TestResult {
    let root = tempfile::tempdir()?;
    let marks = tempfile::tempdir()?;
    let (ready, termed, release, graced) = (
        marks.path().join("ready"),
        marks.path().join("termed"),
        marks.path().join("release"),
        marks.path().join("graced"),
    );
    let registry = JobRegistry::default();
    let service = registry.attach(
        owner("00000000-0000-0000-0000-000000000702")?,
        sessions(root.path())?,
    )?;
    let job = start_script(
        &service,
        &format!(
            "trap 'touch {termed}; while [ ! -e {release} ]; do sleep 0.01; done; \
                 touch {graced}; exit 0' TERM; touch {ready}; while :; do sleep 0.01; done",
            ready = ready.display(),
            termed = termed.display(),
            release = release.display(),
            graced = graced.display(),
        ),
    )?;
    plakous_tools::internal::promote(&service, job)?;
    // The shell must install its TERM trap before the stop; on a slow shell start
    // the signal would otherwise take the default action.
    let started = Instant::now();
    while !ready.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "script never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let id = registry.snapshot()[0].id.clone();
    registry.stop_user(Some(&id))?;
    assert_eq!(registry.snapshot()[0].status, ProcessStatus::Stopping);
    let waited = Instant::now();
    while !termed.exists() {
        assert!(waited.elapsed() < Duration::from_secs(5), "TERM never ran");
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(600));
    assert!(!graced.exists());
    std::fs::write(&release, b"")?;
    registry.settle_user_stops();
    assert!(graced.exists(), "SIGKILL arrived inside the TERM grace");
    assert_eq!(registry.take_user_stop_failures(), [] as [String; 0]);
    assert_eq!(registry.snapshot()[0].status, ProcessStatus::Stopped);
    assert!(
        service
            .jobs()
            .iter()
            .all(|job| job.status == JobStatus::Stopped(JobStopReason::User))
    );
    Ok(())
}

#[test]
fn recent_terminal_snapshot_survives_notice_consumption_and_expires() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let owner = owner("00000000-0000-0000-0000-000000000702")?;
    let service = registry.attach(owner, sessions(root.path())?)?;
    let id = start(&service)?;
    plakous_tools::internal::promote(&service, id)?;
    let now = Instant::now();
    assert_eq!(registry.snapshot_at(now).len(), 1);
    service.stop(id, JobStopReason::User)?;
    let stopped = registry.snapshot_at(now);
    assert_eq!(stopped[0].status, ProcessStatus::Stopped);
    plakous_tools::internal::consume_jobs(&service, &[JoinedProcessSessions::job_number(id)]);
    assert_eq!(
        registry.snapshot_at(now + Duration::from_secs(299)).len(),
        1
    );
    assert_eq!(
        registry.snapshot_at(now + Duration::from_secs(300)),
        [] as [ProcessView; 0]
    );
    Ok(())
}

fn keyed(session: &str, branch: &str) -> TestResult<JobOwner> {
    Ok(JobOwner {
        session_id: SessionId::from_str(session)?,
        branch_id: BranchId::from_str(branch)?,
        agent_id: AgentId::from_str("00000000-0000-0000-0000-000000000703")?,
    })
}

const S1: &str = "00000000-0000-0000-0000-0000000008a1";
const S2: &str = "00000000-0000-0000-0000-0000000008a2";
const A: &str = "00000000-0000-0000-0000-0000000008b1";
const B: &str = "00000000-0000-0000-0000-0000000008b2";
const X: &str = "00000000-0000-0000-0000-0000000008b3";
const C1S: &str = "00000000-0000-0000-0000-0000000008c1";
const C1B: &str = "00000000-0000-0000-0000-0000000008c2";

/// A child of S1 that re-attaches while S2 is current stays on S1.
#[test]
fn child_reattach_under_another_tree_stays_with_its_tree() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    let root_s1 = registry.for_agent("root", S1);
    root_s1.attach(keyed(S1, A)?, sessions(root.path())?)?;
    let child_host = registry.for_agent("c1", S1);
    let child = child_host.attach(keyed(C1S, C1B)?, sessions(root.path())?)?;
    let job = start(&child)?;
    plakous_tools::internal::promote(&child, job)?;
    assert_eq!(registry.snapshot().len(), 1);
    registry
        .for_agent("root", S2)
        .attach(keyed(S2, X)?, sessions(root.path())?)?;
    assert!(registry.snapshot().is_empty(), "S2 does not see S1's child");
    child_host.attach(keyed(C1S, C1B)?, sessions(root.path())?)?;
    assert!(
        registry.snapshot().is_empty(),
        "re-attach does not move the child onto S2"
    );
    root_s1.attach(keyed(S1, A)?, sessions(root.path())?)?;
    assert_eq!(registry.snapshot().len(), 1);
    assert_eq!(registry.snapshot()[0].owner, "c1");
    child.stop(job, JobStopReason::AttemptEnded)?;
    Ok(())
}

/// A child names its own tree. The first root of another tree does not claim it.
#[test]
fn first_root_of_another_tree_does_not_claim_a_child() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    let child = registry
        .for_agent("c1", S1)
        .attach(keyed(C1S, C1B)?, sessions(root.path())?)?;
    let job = start(&child)?;
    plakous_tools::internal::promote(&child, job)?;
    registry
        .for_agent("root", S2)
        .attach(keyed(S2, X)?, sessions(root.path())?)?;
    assert_eq!(registry.snapshot(), [] as [ProcessView; 0]);
    registry
        .for_agent("root", S1)
        .attach(keyed(S1, A)?, sessions(root.path())?)?;
    let rows = registry.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, "c1");
    child.stop(job, JobStopReason::AttemptEnded)?;
    Ok(())
}

/// S1/A, then S2, then S1/B still shows the child. No same-session retarget.
#[test]
fn same_tree_branch_after_a_detour_keeps_the_child() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    registry
        .for_agent("root", S1)
        .attach(keyed(S1, A)?, sessions(root.path())?)?;
    let child = registry
        .for_agent("c1", S1)
        .attach(keyed(C1S, C1B)?, sessions(root.path())?)?;
    let job = start(&child)?;
    plakous_tools::internal::promote(&child, job)?;
    registry
        .for_agent("root", S2)
        .attach(keyed(S2, X)?, sessions(root.path())?)?;
    registry
        .for_agent("root", S1)
        .attach(keyed(S1, B)?, sessions(root.path())?)?;
    let rows = registry.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, "c1");
    child.stop(job, JobStopReason::AttemptEnded)?;
    Ok(())
}

/// Stop-one and stop-all reach only rows in the current snapshot.
#[test]
fn stop_user_does_not_reach_another_trees_jobs() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    registry
        .for_agent("root", S1)
        .attach(keyed(S1, A)?, sessions(root.path())?)?;
    let child = registry
        .for_agent("c1", S1)
        .attach(keyed(C1S, C1B)?, sessions(root.path())?)?;
    let job = start(&child)?;
    plakous_tools::internal::promote(&child, job)?;
    let id = registry.snapshot()[0].id.clone();
    registry
        .for_agent("root", S2)
        .attach(keyed(S2, X)?, sessions(root.path())?)?;
    assert_eq!(registry.snapshot(), [] as [ProcessView; 0]);
    assert!(registry.stop_user(Some(&id)).is_err());
    assert_eq!(registry.running_count(), 1);
    registry.stop_user(None)?;
    registry.settle_user_stops();
    assert_eq!(registry.running_count(), 1);
    child.stop(job, JobStopReason::AttemptEnded)?;
    Ok(())
}

/// Retired children leave the owner maps. A recent row keeps its label
/// until the five-minute window, then both go.
#[test]
fn owner_maps_shrink_after_retired_children() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    registry
        .for_agent("root", S1)
        .attach(keyed(S1, A)?, sessions(root.path())?)?;
    for n in 0..20u32 {
        let branch = format!("00000000-0000-0000-0000-0000000009{n:02x}");
        let service = registry
            .for_agent(format!("c{n}"), S1)
            .attach(keyed(C1S, &branch)?, sessions(root.path())?)?;
        plakous_tools::internal::set_job_owner(&service, format!("c{n}"));
        plakous_tools::internal::close_job_admission(&service, &format!("c{n}"));
    }
    assert_eq!(registry.running_count(), 0);
    let services = registry
        .services
        .lock()
        .map(|services| services.len())
        .unwrap_or(0);
    let (labels, trees) = registry
        .owners
        .lock()
        .map(|owners| (owners.labels.len(), owners.trees.len()))
        .unwrap_or((0, 0));
    assert_eq!((services, labels, trees), (1, 1, 1));

    let kept = registry
        .for_agent("c1", S1)
        .attach(keyed(C1S, C1B)?, sessions(root.path())?)?;
    let job = start(&kept)?;
    plakous_tools::internal::promote(&kept, job)?;
    kept.stop(job, JobStopReason::User)?;
    let child_key = (SessionId::from_str(C1S)?, BranchId::from_str(C1B)?);
    let now = Instant::now();
    assert_eq!(registry.snapshot_at(now).len(), 1);
    registry
        .services
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&child_key);
    assert!(
        registry
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .labels
            .contains_key(&child_key)
    );
    assert_eq!(
        registry.snapshot_at(now + Duration::from_secs(299)).len(),
        1
    );
    assert_eq!(
        registry.snapshot_at(now + Duration::from_secs(300)),
        [] as [ProcessView; 0]
    );
    assert!(
        !registry
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .labels
            .contains_key(&child_key)
    );
    Ok(())
}

fn owner(branch: &str) -> TestResult<JobOwner> {
    Ok(JobOwner {
        session_id: SessionId::from_str("00000000-0000-0000-0000-000000000701")?,
        branch_id: BranchId::from_str(branch)?,
        agent_id: AgentId::from_str("00000000-0000-0000-0000-000000000703")?,
    })
}

fn sessions(root: &Path) -> TestResult<JoinedProcessSessions> {
    Ok(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000704")?),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?
    .joined_process_sessions()?)
}

fn start(service: &JoinedProcessSessions) -> TestResult<plakous::tools::JoinedProcessSessionId> {
    start_script(service, "exec sleep 3600")
}

fn start_script(
    service: &JoinedProcessSessions,
    script: &str,
) -> TestResult<plakous::tools::JoinedProcessSessionId> {
    Ok(service.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(
                JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()])?,
                script,
            )?,
            ".",
            Vec::new(),
            true,
            JoinedProcessTerminalMode::Pipes,
        )?,
        ExecutionOperationControl::new(CancellationToken::new()),
    )?)
}

#[test]
fn first_root_open_sweeps_week_old_files_and_empty_session_directories() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let jobs = state.path().join("jobs");
    let private = std::fs::Permissions::from_mode(0o700);
    let (old_session, empty_session, live, foreign) = (
        jobs.join("00000000-0000-0000-0000-0000000007a1"),
        jobs.join("00000000-0000-0000-0000-0000000007a2"),
        jobs.join("00000000-0000-0000-0000-0000000007a3"),
        jobs.join("not-a-session"),
    );
    for directory in [&jobs, &old_session, &empty_session, &live, &foreign] {
        std::fs::create_dir(directory)?;
        std::fs::set_permissions(directory, private.clone())?;
    }
    let week = std::time::SystemTime::now()
        - plakous_tools::JOB_OUTPUT_STALE_AGE
        - std::time::Duration::from_secs(60);
    let layout = "00000000-0000-0000-0000-000000000702-p1-0123456789abcdef.log";
    let (week_old, fresh) = (old_session.join(layout), live.join(layout));
    // Names outside the job layout are never swept, however old.
    let (unparsed, foreign_file) = (live.join("a-p1-x.log"), foreign.join(layout));
    for file in [&week_old, &unparsed, &foreign_file] {
        std::fs::write(file, b"unconsumed but a week old")?;
        std::fs::File::options()
            .write(true)
            .open(file)?
            .set_modified(week)?;
    }
    std::fs::write(&fresh, b"recent")?;
    let registry = JobRegistry::default();
    registry.set_output_root(jobs.clone());
    let branch = "00000000-0000-0000-0000-000000000702";
    let _service = registry.attach(owner(branch)?, sessions(root.path())?)?;
    assert!(!week_old.exists());
    assert!(!old_session.exists());
    assert!(unparsed.exists() && foreign_file.exists());
    assert!(!empty_session.exists());
    assert!(fresh.exists());
    Ok(())
}

#[test]
fn attached_services_spool_promoted_jobs_under_the_bound_root() -> TestResult {
    let root = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    registry.set_output_root(state.path().join("jobs"));
    let branch = "00000000-0000-0000-0000-000000000702";
    let service = registry.attach(owner(branch)?, sessions(root.path())?)?;
    let job = start(&service)?;
    plakous_tools::internal::promote(&service, job)?;
    let number = JoinedProcessSessions::job_number(job);
    let path = service
        .job_output_file(number)
        .ok_or("promotion issued no output file")?
        .path;
    let directory =
        std::fs::canonicalize(state.path())?.join("jobs/00000000-0000-0000-0000-000000000701");
    let path = Path::new(&path);
    assert_eq!(path.parent(), Some(directory.as_path()));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("name")?;
    let service_hex = name
        .strip_prefix(&format!("{branch}-p{number}-"))
        .and_then(|rest| rest.strip_suffix(".log"))
        .ok_or("unexpected file name")?;
    assert_eq!(service_hex.len(), 16);
    assert!(path.is_file());
    service.stop(job, JobStopReason::Model)?;
    // Without a bound root nothing is spooled and nothing is issued.
    let unbound = JobRegistry::default();
    let service = unbound.attach(owner(branch)?, sessions(root.path())?)?;
    let job = start(&service)?;
    plakous_tools::internal::promote(&service, job)?;
    assert_eq!(
        service.job_output_file(JoinedProcessSessions::job_number(job)),
        None
    );
    service.stop(job, JobStopReason::Model)?;
    Ok(())
}

#[test]
fn terminal_events_retry_after_observation_saturation() -> TestResult {
    use plakous_host_protocol::{Core, SessionObservation};
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let service = registry.attach(
        owner("00000000-0000-0000-0000-000000000702")?,
        sessions(root.path())?,
    )?;
    let job = start(&service)?;
    plakous_tools::internal::promote(&service, job)?;
    service.stop(job, JobStopReason::Model)?;
    plakous_tools::internal::consume_jobs(&service, &[JoinedProcessSessions::job_number(job)]);
    let (wire, received) = plakous_host_protocol::observation_channel::<Core>();
    for _ in 0..plakous_host_protocol::limits::OBSERVATION_QUEUE_CAPACITY {
        wire.send(SessionObservation::JobPromoted {
            job_id: "fill".into(),
            owner: "root".into(),
        })
        .unwrap();
    }
    let sender = crate::runtime::Sender::over(wire);
    let mut events = crate::runtime::job_events::JobEvents::default();
    events.emit(&registry, &sender);
    assert_eq!(registry.events.lock().unwrap().pending.len(), 3);
    received.try_iter().for_each(drop);
    events.emit(&registry, &sender);
    let observed = received.try_iter().collect::<Vec<_>>();
    assert_eq!(observed.len(), 3);
    assert!(matches!(observed[0], SessionObservation::JobStarted { .. }));
    assert!(matches!(
        observed[1],
        SessionObservation::JobPromoted { .. }
    ));
    assert!(matches!(
        observed[2],
        SessionObservation::JobTerminal { .. }
    ));
    assert!(registry.events.lock().unwrap().pending.is_empty());
    Ok(())
}

#[test]
fn lifecycle_queue_reserves_terminal_capacity_before_promotion() -> TestResult {
    let root = tempfile::tempdir()?;
    let service = sessions(root.path())?;
    let job = start(&service)?;
    plakous_tools::internal::promote(&service, job)?;
    let mut view = service.jobs().remove(0);
    let mut queue = JobQueue::default();
    let key = (
        SessionId::from_str("00000000-0000-0000-0000-000000000701")?,
        BranchId::from_str("00000000-0000-0000-0000-000000000702")?,
    );
    for number in 0_u32..256 {
        view.session_id[..4].copy_from_slice(&number.to_le_bytes());
        assert!(queue.record(key, view.clone()));
    }
    view.session_id[..4].copy_from_slice(&256_u32.to_le_bytes());
    assert!(!queue.record(key, view.clone()));
    view.status = JobStatus::Exited(Some(0));
    for number in 0_u32..256 {
        view.session_id[..4].copy_from_slice(&number.to_le_bytes());
        assert!(queue.record(key, view.clone()));
    }
    assert_eq!(queue.pending.len(), JOB_EVENT_CAPACITY);
    assert!(queue.terminals.is_empty());
    service.stop(job, JobStopReason::Model)?;
    Ok(())
}

#[test]
fn terminal_consumed_before_host_poll_still_emits_in_order() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let service = registry.attach(
        owner("00000000-0000-0000-0000-000000000702")?,
        sessions(root.path())?,
    )?;
    let job = start(&service)?;
    plakous_tools::internal::promote(&service, job)?;
    service.stop(job, JobStopReason::Model)?;
    plakous_tools::internal::consume_jobs(&service, &[JoinedProcessSessions::job_number(job)]);
    assert_eq!(registry.views(), [] as [plakous_tools::JobView; 0]);
    let mut observed = Vec::new();
    registry.publish_pending(|phase, _key, job| {
        observed.push(crate::runtime::job_events::transition::<
            plakous_host_protocol::Core,
        >(phase, job));
        true
    });
    assert_eq!(observed.len(), 3);
    assert!(matches!(
        observed[0],
        plakous_host_protocol::SessionObservation::JobStarted { .. }
    ));
    assert!(matches!(
        observed[1],
        plakous_host_protocol::SessionObservation::JobPromoted { .. }
    ));
    assert!(matches!(
        observed[2],
        plakous_host_protocol::SessionObservation::JobTerminal { .. }
    ));
    Ok(())
}

#[test]
fn leftover_jobs_transfer_live_but_cold_reopen_has_no_process_authority() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let key = owner("00000000-0000-0000-0000-000000000702")?;
    let original = registry.attach(key, sessions(root.path())?)?;
    let shell = JoinedProcessShellProfile::new("/bin/sh", [String::from("-c")])?;
    let id = original.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(shell, "sleep 30 & exit 0")?,
            ".",
            Vec::new(),
            false,
            JoinedProcessTerminalMode::Pipes,
        )?,
        ExecutionOperationControl::new(CancellationToken::new()),
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while original.leftover(id).is_none() {
        assert!(
            Instant::now() < deadline,
            "shell never reported its leftover group"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    plakous_tools::internal::promote(&original, id)?;
    let views = original.jobs();
    assert!(registry.is_leftover(&views[0]));
    let replacement = registry.attach(
        JobOwner {
            agent_id: AgentId::from_str("00000000-0000-0000-0000-000000000706")?,
            ..key
        },
        sessions(root.path())?,
    )?;
    assert_eq!(replacement.leftover(id), Some((Some(0), 1)));
    assert_eq!(replacement.running_job_count(), 1);
    let cold = sessions(root.path())?;
    plakous_tools::internal::restore_job_terminals(&cold, &views);
    assert_eq!(cold.jobs()[0].status, JobStatus::OutcomeUnknown);
    assert_eq!(cold.running_job_count(), 0);
    assert!(!cold.retains_session(id));
    cold.stop_jobs(JobStopReason::SessionExit, Duration::from_millis(200))?;
    assert_eq!(replacement.running_job_count(), 1);
    replacement.stop_jobs(JobStopReason::SessionExit, Duration::from_millis(200))?;
    Ok(())
}

#[test]
fn same_branch_transfer_stops_foreground_and_preserves_promoted_service() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let key = owner("00000000-0000-0000-0000-000000000702")?;
    let original = registry.attach(key, sessions(root.path())?)?;
    let foreground = start(&original)?;
    let job = start(&original)?;
    plakous_tools::internal::promote(&original, job)?;
    let next_owner = JobOwner {
        agent_id: AgentId::from_str("00000000-0000-0000-0000-000000000706")?,
        ..key
    };
    let replacement = registry.attach(next_owner, sessions(root.path())?)?;
    assert!(original.joined_terminal_status(foreground).is_some());
    assert!(replacement.retains_session(job));
    assert_eq!(replacement.jobs()[0].status, JobStatus::Running);
    drop(original);
    replacement.stop_jobs(JobStopReason::SessionExit, Duration::from_millis(200))?;
    Ok(())
}

#[test]
fn different_branch_never_receives_source_handles() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let original = registry.attach(
        owner("00000000-0000-0000-0000-000000000702")?,
        sessions(root.path())?,
    )?;
    let job = start(&original)?;
    plakous_tools::internal::promote(&original, job)?;
    let replacement = registry.attach(
        owner("00000000-0000-0000-0000-000000000705")?,
        sessions(root.path())?,
    )?;
    assert!(!replacement.retains_session(job));
    original.stop_jobs(JobStopReason::SessionExit, Duration::from_millis(200))?;
    Ok(())
}

#[test]
fn transition_refusal_counts_only_running_jobs_and_does_not_stop_them() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let key = owner("00000000-0000-0000-0000-000000000702")?;
    let service = registry.attach(key, sessions(root.path())?)?;
    let id = start(&service)?;
    plakous_tools::internal::promote(&service, id)?;
    assert_eq!(
        registry
            .transition_refusal(key.session_id, key.branch_id)
            .as_deref(),
        Some("1 background process is running; stop them first (/ps)"),
    );
    assert_eq!(service.jobs()[0].status, JobStatus::Running);
    service.stop_jobs(JobStopReason::User, Duration::from_millis(200))?;
    assert!(
        registry
            .transition_refusal(key.session_id, key.branch_id)
            .is_none()
    );
    Ok(())
}

#[test]
fn released_empty_slice_is_pruned_but_pending_notices_and_running_jobs_are_retained() -> TestResult
{
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let key = owner("00000000-0000-0000-0000-000000000702")?;
    let service = registry.attach(key, sessions(root.path())?)?;
    plakous_tools::internal::set_job_owner(&service, key.agent_id.to_string());
    let id = start(&service)?;
    plakous_tools::internal::promote(&service, id)?;
    plakous_tools::internal::close_job_admission(&service, &key.agent_id.to_string());
    assert_eq!(registry.running_count(), 1);
    assert_eq!(registry.services.lock().map_err(|_| "registry")?.len(), 1);
    service.stop_jobs(JobStopReason::SessionExit, Duration::from_millis(200))?;
    assert_eq!(registry.running_count(), 0);
    assert_eq!(registry.services.lock().map_err(|_| "registry")?.len(), 1);
    plakous_tools::internal::consume_jobs(&service, &[service.jobs()[0].number]);
    assert_eq!(registry.running_count(), 0);
    assert!(registry.services.lock().map_err(|_| "registry")?.is_empty());
    Ok(())
}

#[tokio::test]
async fn host_release_stops_all_registered_slices() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = JobRegistry::default();
    let mut services = Vec::new();
    for branch in [
        "00000000-0000-0000-0000-000000000702",
        "00000000-0000-0000-0000-000000000705",
    ] {
        let service = registry.attach(owner(branch)?, sessions(root.path())?)?;
        let job = start(&service)?;
        plakous_tools::internal::promote(&service, job)?;
        services.push(service);
    }
    registry.stop_all(Duration::from_millis(200)).await?;
    for service in services {
        assert_eq!(
            service.jobs()[0].status,
            JobStatus::Stopped(JobStopReason::SessionExit)
        );
    }
    Ok(())
}

#[test]
fn terminal_admission_gate_and_stale_owner_cleanup_are_fenced() -> TestResult {
    let root = tempfile::tempdir()?;
    let service = sessions(root.path())?;
    plakous_tools::internal::set_job_owner(&service, "old-owner".to_owned());
    let job = start(&service)?;
    plakous_tools::internal::promote(&service, job)?;
    let foreground = start(&service)?;
    plakous_tools::internal::close_job_admission(&service, "old-owner");
    assert!(plakous_tools::internal::promote(&service, foreground).is_err());
    assert!(start(&service).is_err());
    service.stop_foreground()?;
    plakous_tools::internal::set_job_owner(&service, "new-owner".to_owned());
    plakous_tools::internal::close_job_admission(&service, "old-owner");
    plakous_tools::internal::stop_jobs_for_owner(
        &service,
        "old-owner",
        JobStopReason::SessionExit,
        Duration::from_millis(200),
    )?;
    assert_eq!(service.jobs()[0].status, JobStatus::Running);
    let replacement_job = start(&service)?;
    plakous_tools::internal::promote(&service, replacement_job)?;
    service.stop_jobs(JobStopReason::SessionExit, Duration::from_millis(200))?;
    Ok(())
}

fn drain_job_events(
    registry: &JobRegistry,
    events: &mut crate::runtime::job_events::JobEvents,
) -> (
    Vec<plakous_host_protocol::SessionObservation<plakous_host_protocol::Core>>,
    Vec<crate::sessions::TranscriptRow>,
) {
    let (wire, received) =
        plakous_host_protocol::observation_channel::<plakous_host_protocol::Core>();
    let sender = crate::runtime::Sender::over(wire);
    let rows = events.emit(registry, &sender);
    (received.try_iter().collect(), rows)
}

/// A child terminal is observed and not transcribed. A root terminal still
/// persists one process row, including a job whose branch is an earlier
/// root attach after the root has forked or rebound.
#[test]
fn child_owned_terminal_yields_no_process_row_and_root_lineage_yields_one() -> TestResult {
    use crate::sessions::TranscriptRow;
    use plakous_host_protocol::SessionObservation;
    use plakous_host_protocol::views::transcript::{ProcessNotice, ProcessOutcome};

    let root = tempfile::tempdir()?;
    let registry = Arc::new(JobRegistry::default());
    let root_host = registry.for_agent("root", "s");
    let predecessor = root_host.attach(
        owner("00000000-0000-0000-0000-000000000702")?,
        sessions(root.path())?,
    )?;
    // A still-running sibling keeps the terminal job's id distinct from the
    // child's, which has its own service counter.
    let kept = start(&predecessor)?;
    plakous_tools::internal::promote(&predecessor, kept)?;
    let predecessor_job = start(&predecessor)?;
    plakous_tools::internal::promote(&predecessor, predecessor_job)?;
    // The root's own fork/rebind moves the current root key. The job stays
    // on the predecessor branch, which is still the root's lineage.
    let current = root_host.attach(
        owner("00000000-0000-0000-0000-000000000705")?,
        sessions(root.path())?,
    )?;
    predecessor.stop(predecessor_job, JobStopReason::Model)?;
    let predecessor_id = format!("p{}", JoinedProcessSessions::job_number(predecessor_job));

    let child_host = registry.for_agent("c7", "s");
    let child = child_host.attach(
        owner("00000000-0000-0000-0000-000000000708")?,
        sessions(root.path())?,
    )?;
    let child_job = start(&child)?;
    plakous_tools::internal::promote(&child, child_job)?;
    child.stop(child_job, JobStopReason::User)?;
    let child_id = format!("p{}", JoinedProcessSessions::job_number(child_job));
    assert_ne!(predecessor_id, child_id);

    // First publish is attach-time: both terminals are already queued, and
    // snapshot reconciliation runs because no generation was observed yet.
    let mut events = crate::runtime::job_events::JobEvents::default();
    let (observed, rows) = drain_job_events(&registry, &mut events);
    let (_, again) = drain_job_events(&registry, &mut events);

    let current_job = start(&current)?;
    plakous_tools::internal::promote(&current, current_job)?;
    current.stop(current_job, JobStopReason::Model)?;
    let current_id = format!("p{}", JoinedProcessSessions::job_number(current_job));
    let (current_observed, current_rows) = drain_job_events(&registry, &mut events);
    predecessor.stop_jobs(JobStopReason::SessionExit, Duration::from_millis(200))?;
    current.stop_jobs(JobStopReason::SessionExit, Duration::from_millis(200))?;
    child.stop_jobs(JobStopReason::SessionExit, Duration::from_millis(200))?;

    let terminals = |observed: &[SessionObservation<plakous_host_protocol::Core>]| {
        observed
            .iter()
            .filter_map(|event| match event {
                SessionObservation::JobTerminal { job_id, .. } => Some(job_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let mut got = terminals(&observed);
    got.sort();
    let mut expected = vec![predecessor_id.clone(), child_id.clone()];
    expected.sort();
    assert_eq!(got, expected);
    assert!(observed.iter().any(|event| matches!(
        event,
        SessionObservation::JobStarted { job_id, .. } if job_id == &child_id
    )));
    assert!(observed.iter().any(|event| matches!(
        event,
        SessionObservation::JobPromoted { job_id, .. } if job_id == &child_id
    )));
    assert_eq!(rows.len(), 1, "only the root-lineage terminal persists");
    assert!(matches!(
        &rows[0],
        TranscriptRow::Process(ProcessNotice {
            job_id,
            outcome: ProcessOutcome::Stopped,
            duration_ms: Some(_),
            ..
        }) if job_id == &predecessor_id
    ));
    assert_eq!(again, [] as [TranscriptRow; 0]);

    assert_eq!(terminals(&current_observed), vec![current_id.clone()]);
    assert_eq!(current_rows.len(), 1);
    assert!(matches!(
        &current_rows[0],
        TranscriptRow::Process(ProcessNotice {
            job_id,
            outcome: ProcessOutcome::Stopped,
            ..
        }) if job_id == &current_id
    ));
    Ok(())
}
