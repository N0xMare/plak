//! Process services retained across same-branch owner replacement.

use plakous::tools::{
    ExecutionBackendError, JobHost, JobHostCapacity, JobOutputRoot, JobOwner, JoinedProcessSessions,
};
use plakous_host_protocol::views::{ProcessStatus, ProcessView};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type BranchKey = (plakous::model::SessionId, plakous::model::BranchId);

fn project_job(
    key: BranchKey,
    job: plakous::tools::JobView,
    owner: Option<&String>,
) -> ProcessView {
    ProcessView {
        id: format!("{}:{}:p{}", key.0, key.1, job.number),
        command: job.command.split_whitespace().collect::<Vec<_>>().join(" "),
        owner: owner.cloned().unwrap_or(job.owner),
        started_at: job.started_at.seconds(),
        elapsed_ms: u64::try_from(job.elapsed.as_millis()).unwrap_or(u64::MAX),
        status: match job.status {
            plakous::tools::JobStatus::Running => ProcessStatus::Running,
            plakous::tools::JobStatus::Exited(code) => ProcessStatus::Exited(code),
            plakous::tools::JobStatus::Stopped(_) => ProcessStatus::Stopped,
            plakous::tools::JobStatus::OutcomeUnknown => ProcessStatus::OutcomeUnknown,
        },
        output: job.output,
    }
}

#[derive(Default)]
struct ProcessOwners {
    labels: HashMap<BranchKey, String>,
    /// Product tree the branch was built for. Not the root that happened to
    /// be current at attach.
    trees: HashMap<BranchKey, String>,
    root: Option<BranchKey>,
}

impl ProcessOwners {
    fn visible(&self, key: BranchKey) -> bool {
        let Some(root) = self.root else {
            return true;
        };
        let Some(tree) = self.trees.get(&key) else {
            return true;
        };
        if self.trees.get(&root).is_none_or(|current| current != tree) {
            return false;
        }
        // An earlier branch of this tree keeps its own rows. Children of the
        // tree stay with it across a fork or rebind of the current root.
        self.labels
            .get(&key)
            .is_none_or(|label| label != "root" || key == root)
    }
}

fn retain_owner_maps(
    owners: &mut ProcessOwners,
    recent: &HashMap<String, (BranchKey, ProcessView, Instant)>,
    live: &BTreeSet<BranchKey>,
    now: Instant,
) {
    let root = owners.root;
    let keep = |key: &BranchKey| {
        live.contains(key)
            || root == Some(*key)
            || recent.values().any(|(branch, _, finished)| {
                branch == key && now.saturating_duration_since(*finished) < Duration::from_secs(300)
            })
    };
    owners.labels.retain(|key, _| keep(key));
    owners.trees.retain(|key, _| keep(key));
}

fn release_stopping_rows(stopping: &mut HashMap<String, usize>, rows: &[String]) {
    for row in rows {
        if let Some(count) = stopping.get_mut(row) {
            *count -= 1;
            if *count == 0 {
                stopping.remove(row);
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct JobRegistry {
    services: Mutex<HashMap<BranchKey, JoinedProcessSessions>>,
    capacity: JobHostCapacity,
    events: Arc<Mutex<JobQueue>>,
    owners: Mutex<ProcessOwners>,
    recent: Mutex<HashMap<String, (BranchKey, ProcessView, Instant)>>,
    user_stops: Arc<Mutex<UserStops>>,
    output_root: std::sync::OnceLock<std::path::PathBuf>,
    opened_output_root: std::sync::OnceLock<Option<JobOutputRoot>>,
}

/// The TERM grace a user stop grants before SIGKILL, as TaskStop documents.
const USER_STOP_GRACE: Duration = Duration::from_secs(2);

/// User stops run off the caller's thread so the grace never blocks a host
/// loop. Rows stay `Stopping` until their stop has joined the process group.
#[derive(Default)]
struct UserStops {
    rows: HashMap<String, usize>,
    failures: Vec<String>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

enum UserStopTarget {
    One(JoinedProcessSessions, u32),
    All(JoinedProcessSessions),
}

struct AttributedJobHost {
    registry: Arc<JobRegistry>,
    owner: String,
    /// The product tree this host belongs to. A child carries its source
    /// tree, so a later attach cannot move it onto whichever root is current.
    tree: String,
}

impl JobHost for AttributedJobHost {
    fn service(
        &self,
        session: plakous::model::SessionId,
        branch: plakous::model::BranchId,
    ) -> Option<JoinedProcessSessions> {
        self.registry.service(session, branch)
    }

    fn observe_terminal(&self, owner: JobOwner, jobs: &[plakous::tools::JobView]) {
        self.registry.observe_terminal(owner, jobs);
    }
    fn attach(
        &self,
        owner: JobOwner,
        fresh: JoinedProcessSessions,
    ) -> Result<JoinedProcessSessions, ExecutionBackendError> {
        let service = self.registry.attach(owner, fresh)?;
        let key = (owner.session_id, owner.branch_id);
        let mut owners = self
            .registry
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.owner == "root" {
            owners.root = Some(key);
        }
        owners.trees.insert(key, self.tree.clone());
        owners.labels.insert(key, self.owner.clone());
        Ok(service)
    }
}

const JOB_EVENT_CAPACITY: usize = 768;

#[derive(Default)]
struct JobQueue {
    pending: VecDeque<(u8, BranchKey, plakous::tools::JobView)>,
    terminals: BTreeSet<[u8; 16]>,
    generation: u64,
}

impl JobQueue {
    /// Whether a promotion's three lifecycle events fit now.
    fn has_room(&self) -> bool {
        self.pending.len() + self.terminals.len() + 3 <= JOB_EVENT_CAPACITY
    }

    fn record(&mut self, key: BranchKey, job: plakous::tools::JobView) -> bool {
        if job.status == plakous::tools::JobStatus::Running {
            // Reserve all three lifecycle events at promotion. Terminal delivery
            // cannot fail later, even if the host has not drained a single event.
            if !self.has_room() {
                return false;
            }
            self.terminals.insert(job.session_id);
            self.pending.push_back((0, key, job.clone()));
            self.pending.push_back((1, key, job));
        } else if self.terminals.remove(&job.session_id) {
            self.pending.push_back((2, key, job));
        }
        self.generation = self.generation.wrapping_add(1);
        true
    }
}

impl JobRegistry {
    /// Names the private directory promoted jobs spool their output under.
    /// Only the first binding counts; it is opened on the first attach.
    pub(crate) fn set_output_root(&self, root: std::path::PathBuf) {
        let _ = self.output_root.set(root);
    }

    fn output_root(&self) -> Option<JobOutputRoot> {
        self.opened_output_root
            .get_or_init(|| {
                let path = self.output_root.get()?;
                // Without a usable private root, promotion simply issues no file.
                let root = JobOutputRoot::open(path).ok()?;
                root.sweep_stale(std::time::SystemTime::now());
                Some(root)
            })
            .clone()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }

    pub(crate) fn retain_reported(&self, reported: &mut HashMap<[u8; 16], u8>) {
        let queue = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reported.retain(|id, _| {
            queue.terminals.contains(id)
                || queue
                    .pending
                    .iter()
                    .any(|(_, _, job)| job.session_id == *id)
        });
    }

    /// Branches attached as the root agent. An earlier fork or rebind of that
    /// root keeps its label, so the lineage stays root-owned after the current
    /// root key moves. A label is written at attach, before the branch can
    /// queue a job, so a set read before [`Self::publish_pending`] covers every
    /// queued job.
    ///
    /// Read it before publishing, never inside the publish callback: the
    /// process reaper records into `events` while holding a service's state
    /// lock, and `snapshot_at` holds `owners` while reading that state, so
    /// taking `owners` under `events` would close a lock cycle.
    pub(crate) fn root_branches(&self) -> std::collections::HashSet<BranchKey> {
        self.owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .labels
            .iter()
            .filter(|(_, label)| label.as_str() == "root")
            .map(|(key, _)| *key)
            .collect()
    }

    pub(crate) fn publish_pending(
        &self,
        mut publish: impl FnMut(u8, BranchKey, &plakous::tools::JobView) -> bool,
    ) {
        let mut queue = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some((phase, key, job)) = queue.pending.front() {
            if !publish(*phase, *key, job) {
                break;
            }
            queue.pending.pop_front();
        }
    }

    fn capture_terminals(&self, owner: JobOwner, jobs: &[plakous::tools::JobView], now: Instant) {
        let key = (owner.session_id, owner.branch_id);
        let owners = self
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut recent = self
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in jobs
            .iter()
            .filter(|job| job.status != plakous::tools::JobStatus::Running)
        {
            let row = project_job(key, job.clone(), owners.labels.get(&key));
            recent.entry(row.id.clone()).or_insert((key, row, now));
        }
    }
    /// `tree` is the product tree this agent belongs to (spec 03: a tree is
    /// keyed by its session). Root and child of one tree pass the same id.
    /// A child never inherits whichever root is current.
    pub(crate) fn for_agent(
        self: &Arc<Self>,
        owner: impl Into<String>,
        tree: impl Into<String>,
    ) -> Arc<dyn JobHost> {
        Arc::new(AttributedJobHost {
            registry: self.clone(),
            owner: owner.into(),
            tree: tree.into(),
        })
    }

    /// The tree of the root currently selected, when one has attached.
    ///
    /// Offline child bindings read this so they join the root that is already
    /// current. Production children pass their tree into [`Self::for_agent`].
    #[cfg(feature = "test-support")]
    pub(crate) fn current_tree(&self) -> Option<String> {
        let owners = self
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = owners.root?;
        owners.trees.get(&root).cloned()
    }

    pub(crate) fn snapshot(&self) -> Vec<ProcessView> {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> Vec<ProcessView> {
        let services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut recent = self
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stopping = self
            .user_stops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rows
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut rows = Vec::new();
        for (key, service) in services.iter() {
            for job in service.jobs() {
                let mut row = project_job(*key, job, owners.labels.get(key));
                if row.status == ProcessStatus::Running && stopping.contains(&row.id) {
                    row.status = ProcessStatus::Stopping;
                }
                if row.status == ProcessStatus::Running || row.status == ProcessStatus::Stopping {
                    if owners.visible(*key) {
                        rows.push(row);
                    }
                } else {
                    recent.entry(row.id.clone()).or_insert((*key, row, now));
                }
            }
        }
        rows.extend(
            recent
                .values()
                .filter(|(key, _, finished)| {
                    owners.visible(*key)
                        && now.saturating_duration_since(*finished) < Duration::from_secs(300)
                })
                .map(|(_, row, _)| row.clone()),
        );
        // Keep the expiry tombstone while the service still exposes its pending notice.
        recent.retain(|id, (_, _, finished)| {
            now.saturating_duration_since(*finished) < Duration::from_secs(300)
                || services.iter().any(|(key, service)| {
                    service
                        .jobs()
                        .iter()
                        .any(|job| *id == format!("{}:{}:p{}", key.0, key.1, job.number))
                })
        });
        let live = services.keys().copied().collect::<BTreeSet<_>>();
        retain_owner_maps(&mut owners, &recent, &live, now);
        rows.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        rows
    }

    /// Starts a user stop of one visible row, or of every visible job, and
    /// returns once it is underway. Each target gets [`USER_STOP_GRACE`]
    /// between SIGTERM and SIGKILL; its row reads `Stopping` meanwhile.
    /// Failures surface through [`Self::take_user_stop_failures`].
    pub(crate) fn stop_user(&self, id: Option<&str>) -> Result<(), String> {
        // Only rows the current snapshot shows. A hidden other-tree job is
        // not stopped by this session's stop-one or stop-all.
        let services = {
            let services = self
                .services
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owners = self
                .owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            services
                .iter()
                .filter(|(key, _)| owners.visible(**key))
                .map(|(key, service)| (*key, service.clone()))
                .collect::<Vec<_>>()
        };
        let mut targets = Vec::new();
        let mut rows = Vec::new();
        for (key, service) in services {
            let row_id = |number: u32| format!("{}:{}:p{}", key.0, key.1, number);
            let Some(id) = id else {
                rows.extend(
                    service
                        .jobs()
                        .into_iter()
                        .filter(|job| job.status == plakous::tools::JobStatus::Running)
                        .map(|job| row_id(job.number)),
                );
                targets.push(UserStopTarget::All(service));
                continue;
            };
            let Some(number) = service
                .jobs()
                .into_iter()
                .map(|job| job.number)
                .find(|number| row_id(*number) == id)
            else {
                continue;
            };
            if service.session_for_number(number).is_none() {
                // Already retired: the stop has nothing left to do.
                return Ok(());
            }
            rows.push(row_id(number));
            targets.push(UserStopTarget::One(service, number));
            break;
        }
        if id.is_some() && targets.is_empty() {
            return Err("background process is no longer available".into());
        }
        self.spawn_user_stop(targets, rows)
    }

    fn spawn_user_stop(
        &self,
        targets: Vec<UserStopTarget>,
        rows: Vec<String>,
    ) -> Result<(), String> {
        let mut stops = self
            .user_stops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stops.workers.retain(|worker| !worker.is_finished());
        for row in &rows {
            *stops.rows.entry(row.clone()).or_default() += 1;
        }
        let shared = Arc::clone(&self.user_stops);
        let worker_rows = rows.clone();
        let spawned = std::thread::Builder::new()
            .name("plak-user-stop".into())
            .spawn(move || {
                let workers = targets
                    .into_iter()
                    .map(|target| {
                        std::thread::Builder::new()
                            .name("plak-user-stop-job".into())
                            .spawn(move || match target {
                                UserStopTarget::One(service, number) => {
                                    service.session_for_number(number).map_or(Ok(()), |handle| {
                                        settle_numbered_stop(
                                            service.stop_with_grace(
                                                handle,
                                                plakous::tools::JobStopReason::User,
                                                USER_STOP_GRACE,
                                            ),
                                            service.session_for_number(number) != Some(handle),
                                        )
                                    })
                                }
                                UserStopTarget::All(service) => service.stop_jobs(
                                    plakous::tools::JobStopReason::User,
                                    USER_STOP_GRACE,
                                ),
                            })
                    })
                    .collect::<Vec<_>>();
                let mut failure = None;
                for worker in workers {
                    let Ok(worker) = worker else {
                        failure.get_or_insert_with(|| {
                            "background process stop could not start".to_owned()
                        });
                        continue;
                    };
                    match worker.join() {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            failure.get_or_insert_with(|| error.to_string());
                        }
                        Err(_) => {
                            failure.get_or_insert_with(|| {
                                "background process stop task failed".to_owned()
                            });
                        }
                    }
                }
                let mut stops = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                release_stopping_rows(&mut stops.rows, &worker_rows);
                stops.failures.extend(failure);
            });
        let Ok(worker) = spawned else {
            release_stopping_rows(&mut stops.rows, &rows);
            return Err("background process stop could not start".into());
        };
        stops.workers.push(worker);
        Ok(())
    }

    /// Failures of user stops that finished since the last call.
    pub(crate) fn take_user_stop_failures(&self) -> Vec<String> {
        std::mem::take(
            &mut self
                .user_stops
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .failures,
        )
    }

    /// Joins every user stop started so far.
    #[cfg(test)]
    fn settle_user_stops(&self) {
        let workers = std::mem::take(
            &mut self
                .user_stops
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .workers,
        );
        for worker in workers {
            let _ = worker.join();
        }
    }

    pub(crate) fn views(&self) -> Vec<plakous::tools::JobView> {
        let services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        services
            .iter()
            .flat_map(JoinedProcessSessions::jobs)
            .collect()
    }
    pub(crate) fn running_count(&self) -> usize {
        let live = {
            let mut services = self
                .services
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            services.retain(|_, service| !service.job_admission_closed() || service.has_jobs());
            let live = services.keys().copied().collect::<BTreeSet<_>>();
            let count = services
                .values()
                .map(JoinedProcessSessions::running_job_count)
                .sum();
            (live, count)
        };
        self.prune_owner_maps(&live.0, Instant::now());
        live.1
    }

    /// Drops owner rows for services that are gone and whose recent terminal
    /// rows have left the five-minute window.
    fn prune_owner_maps(&self, live: &BTreeSet<BranchKey>, now: Instant) {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recent = self
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retain_owner_maps(&mut owners, &recent, live, now);
    }
    pub(crate) async fn stop_all(&self, grace: Duration) -> Result<(), String> {
        self.stop_all_except(grace, &[]).await
    }

    pub(crate) fn is_leftover(&self, job: &plakous::tools::JobView) -> bool {
        self.services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|service| {
                service.jobs().iter().any(|view| {
                    view.session_id == job.session_id
                        && view.owner_generation == job.owner_generation
                }) && service
                    .session_for_number(job.number)
                    .is_some_and(|id| service.leftover(id).is_some())
            })
    }

    pub(crate) async fn disown_leftovers(&self) -> Result<(), String> {
        let services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut workers = tokio::task::JoinSet::new();
        for service in services {
            workers.spawn_blocking(move || service.disown_leftovers());
        }
        let mut failures = Vec::new();
        while let Some(result) = workers.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(error.to_string()),
                Err(_) => failures.push("job disown task failed".to_owned()),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    pub(crate) async fn stop_all_except(
        &self,
        grace: Duration,
        owners: &[String],
    ) -> Result<(), String> {
        let services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|service| {
                service.jobs().iter().any(|job| {
                    job.status == plakous::tools::JobStatus::Running && !owners.contains(&job.owner)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut stops = tokio::task::JoinSet::new();
        for service in services {
            stops.spawn_blocking(move || {
                service.stop_jobs(plakous::tools::JobStopReason::SessionExit, grace)
            });
        }
        let mut failure = None;
        while let Some(result) = stops.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failure.get_or_insert_with(|| error.to_string());
                }
                Err(_) => {
                    failure.get_or_insert_with(|| "background process stop task failed".to_owned());
                }
            }
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        Ok(())
    }
    pub(crate) fn transition_refusal(
        &self,
        session: plakous::model::SessionId,
        branch: plakous::model::BranchId,
    ) -> Option<String> {
        let services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = services.get(&(session, branch)).map_or(0, |service| {
            service
                .jobs()
                .iter()
                .filter(|job| job.status == plakous::tools::JobStatus::Running)
                .count()
        });
        (count != 0).then(|| {
            let noun = if count == 1 {
                "process is"
            } else {
                "processes are"
            };
            format!("{count} background {noun} running; stop them first (/ps)")
        })
    }
}

impl JobHost for JobRegistry {
    fn service(
        &self,
        session: plakous::model::SessionId,
        branch: plakous::model::BranchId,
    ) -> Option<JoinedProcessSessions> {
        self.services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(session, branch))
            .cloned()
    }

    fn observe_terminal(&self, owner: JobOwner, jobs: &[plakous::tools::JobView]) {
        self.capture_terminals(owner, jobs, Instant::now());
        let key = (owner.session_id, owner.branch_id);
        let mut queue = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in jobs {
            queue.record(key, job.clone());
        }
    }
    fn attach(
        &self,
        owner: JobOwner,
        fresh: JoinedProcessSessions,
    ) -> Result<JoinedProcessSessions, ExecutionBackendError> {
        // The first open sweeps the whole root: never under the services lock.
        let output_root = self.output_root();
        let mut services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let service = services
            .entry((owner.session_id, owner.branch_id))
            .or_insert_with(|| {
                fresh.bind_host_capacity(self.capacity.clone());
                if let Some(root) = output_root {
                    fresh.bind_job_outputs(root, owner.session_id, owner.branch_id);
                }
                let queue = Arc::clone(&self.events);
                let room = Arc::clone(&self.events);
                let key = (owner.session_id, owner.branch_id);
                fresh.observe_jobs(
                    move |job| {
                        queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .record(key, job)
                    },
                    move || {
                        room.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .has_room()
                    },
                );
                fresh
            })
            .clone();
        drop(services);
        // Foreground calls belong to the old turn, never the replacement owner.
        service.stop_foreground()?;
        Ok(service)
    }
}

/// Settles one numbered job's user stop. A job that retired after its number
/// resolved (`retired`) has nothing left to stop, so its stop error is not a
/// failure.
fn settle_numbered_stop<T>(
    result: Result<T, ExecutionBackendError>,
    retired: bool,
) -> Result<(), ExecutionBackendError> {
    match result {
        Err(_) if retired => Ok(()),
        result => result.map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
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
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000704",
            )?),
            root,
            ".",
            ExecutionEnvironmentConfig::default(),
        )?
        .joined_process_sessions()?)
    }

    fn start(
        service: &JoinedProcessSessions,
    ) -> TestResult<plakous::tools::JoinedProcessSessionId> {
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
    fn released_empty_slice_is_pruned_but_pending_notices_and_running_jobs_are_retained()
    -> TestResult {
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
}
