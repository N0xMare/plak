//! Process services keyed by agent line: retained across a same-branch owner
//! replacement and handed to the line's next head on a rebind.

use plakous::tools::{
    ExecutionBackendError, JobHost, JobHostCapacity, JobOutputRoot, JobOwner, JoinedSessionsPort,
};
use plakous_host_protocol::views::{ProcessStatus, ProcessView};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type BranchKey = (plakous::model::SessionId, plakous::model::BranchId);

/// One line's service and the head that currently owns it.
struct LineService {
    owner: JobOwner,
    service: JoinedSessionsPort,
}

impl LineService {
    /// The owning head's branch, under which its rows are shown.
    const fn key(&self) -> BranchKey {
        (self.owner.session_id, self.owner.branch_id)
    }
}

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
            plakous::tools::JobStatus::Released => ProcessStatus::Released,
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
    services: Mutex<HashMap<plakous::model::LineId, LineService>>,
    /// The bound on running process starts, shared with every registry of
    /// the process.
    capacity: Arc<JobHostCapacity>,
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
    One(JoinedSessionsPort, u32),
    All(JoinedSessionsPort),
}

struct AttributedJobHost {
    registry: Arc<JobRegistry>,
    owner: String,
    /// The product tree this host belongs to. A child carries its source
    /// tree, so a later attach cannot move it onto whichever root is current.
    tree: String,
}

impl JobHost for AttributedJobHost {
    fn service(&self, line: plakous::model::LineId) -> Option<JoinedSessionsPort> {
        self.registry.service(line)
    }

    fn service_at(
        &self,
        session: plakous::model::SessionId,
        branch: plakous::model::BranchId,
    ) -> Option<JoinedSessionsPort> {
        self.registry.service_at(session, branch)
    }

    fn observe_terminal(&self, owner: JobOwner, jobs: &[plakous::tools::JobView]) {
        self.registry.observe_terminal(owner, jobs);
    }
    fn attach(
        &self,
        owner: JobOwner,
        fresh: JoinedSessionsPort,
    ) -> Result<JoinedSessionsPort, ExecutionBackendError> {
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
    /// A registry of one session's jobs under `capacity`, the process's
    /// bound on running process starts.
    pub(crate) fn under(capacity: Arc<JobHostCapacity>) -> Self {
        Self {
            capacity,
            ..Self::default()
        }
    }

    /// Whether this registry is under `capacity`.
    pub(crate) fn is_under(&self, capacity: &Arc<JobHostCapacity>) -> bool {
        Arc::ptr_eq(&self.capacity, capacity)
    }

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
        for entry in services.values() {
            let key = &entry.key();
            for job in entry.service.jobs() {
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
                || services.values().any(|entry| {
                    let key = entry.key();
                    entry
                        .service
                        .jobs()
                        .iter()
                        .any(|job| *id == format!("{}:{}:p{}", key.0, key.1, job.number))
                })
        });
        let live = services
            .values()
            .map(LineService::key)
            .collect::<BTreeSet<_>>();
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
        self.stop_owned(None, id)
    }

    /// [`Self::stop_user`] over one node's own jobs only: the services its
    /// lines' heads attached as the node `agent` of the tree `tree`. A
    /// child's id names it only within its tree.
    pub(crate) fn stop_node(
        &self,
        tree: &str,
        agent: &str,
        id: Option<&str>,
    ) -> Result<(), String> {
        self.stop_owned(Some((tree, agent)), id)
    }

    fn stop_owned(&self, node: Option<(&str, &str)>, id: Option<&str>) -> Result<(), String> {
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
                .values()
                .filter(|entry| {
                    let key = entry.key();
                    match node {
                        None => owners.visible(key),
                        Some((tree, agent)) => {
                            owners.trees.get(&key).map(String::as_str) == Some(tree)
                                && owners.labels.get(&key).map(String::as_str) == Some(agent)
                        }
                    }
                })
                .map(|entry| (entry.key(), entry.service.clone()))
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
            .map(|entry| entry.service.clone())
            .collect::<Vec<_>>();
        services.iter().flat_map(JoinedSessionsPort::jobs).collect()
    }
    pub(crate) fn running_count(&self) -> usize {
        let live = {
            let mut services = self
                .services
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            services.retain(|_, entry| {
                !entry.service.job_admission_closed() || entry.service.has_jobs()
            });
            let live = services
                .values()
                .map(LineService::key)
                .collect::<BTreeSet<_>>();
            let count = services
                .values()
                .map(|entry| entry.service.running_job_count())
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
            .map(|entry| &entry.service)
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
            .map(|entry| entry.service.clone())
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
            .map(|entry| &entry.service)
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
    /// The service of the line whose head is `(session, branch)`.
    #[cfg(test)]
    pub(crate) fn service_of_head(
        &self,
        session: plakous::model::SessionId,
        branch: plakous::model::BranchId,
    ) -> Option<JoinedSessionsPort> {
        self.services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|entry| entry.key() == (session, branch))
            .map(|entry| entry.service.clone())
    }

    pub(crate) fn transition_refusal(
        &self,
        session: plakous::model::SessionId,
        branch: plakous::model::BranchId,
    ) -> Option<RunningProcesses> {
        let services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = services
            .values()
            .find(|entry| entry.key() == (session, branch))
            .map_or(0, |entry| {
                entry
                    .service
                    .jobs()
                    .iter()
                    .filter(|job| job.status == plakous::tools::JobStatus::Running)
                    .count()
            });
        (count != 0).then_some(RunningProcesses { count })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunningProcesses {
    pub(crate) count: usize,
}

impl std::fmt::Display for RunningProcesses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let noun = if self.count == 1 {
            "process is"
        } else {
            "processes are"
        };
        write!(
            f,
            "{} background {noun} running; stop them before changing the session",
            self.count
        )
    }
}

impl JobHost for JobRegistry {
    fn service(&self, line: plakous::model::LineId) -> Option<JoinedSessionsPort> {
        self.services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&line)
            .map(|entry| entry.service.clone())
    }

    fn service_at(
        &self,
        session: plakous::model::SessionId,
        branch: plakous::model::BranchId,
    ) -> Option<JoinedSessionsPort> {
        self.services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|entry| entry.key() == (session, branch))
            .map(|entry| entry.service.clone())
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
        fresh: JoinedSessionsPort,
    ) -> Result<JoinedSessionsPort, ExecutionBackendError> {
        // The first open sweeps the whole root: never under the services lock.
        let output_root = self.output_root();
        let mut services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The line's next head, or the same head reopened, takes the service
        // with its running jobs; an older owner never does.
        let service = if let Some(entry) = services.get_mut(&owner.line) {
            if !owner.succeeds(&entry.owner) {
                return Err(ExecutionBackendError::stale_job_owner());
            }
            entry.owner = owner;
            entry.service.clone()
        } else {
            fresh.bind_host_capacity((*self.capacity).clone());
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
            services.insert(
                owner.line,
                LineService {
                    owner,
                    service: fresh.clone(),
                },
            );
            fresh
        };
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
mod tests;
