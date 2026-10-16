//! Unpublished process-test access to Plak's real product journal.

#![allow(missing_docs, reason = "private cfg(test)-only process-test seam")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use plakous::agent::ReasoningEffort;
use plakous::sessions::{SessionLocator, SessionRuntimeBinding, SessionTargetBinding};
use plakous::subscriptions::NativeProvider;

use crate::product_host::{
    self, HostConfig, TestHostBoundary, TestHostInjection, TestHostWorkflow, TestRuntimeRole,
};
use crate::product_paths::PlakPaths;
use crate::product_sessions::{self, QueueDrainState, QueueItem, SessionRecord};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalSnapshot {
    pub product_session_id: String,
    pub locator: Option<String>,
    pub queue_items: usize,
    pub queue_paused: bool,
    pub source_product_session_id: Option<String>,
    pub source_workflow_operation_id: Option<String>,
}

pub struct ProductJournal {
    paths: PlakPaths,
    record: SessionRecord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InjectedRuntimeRole {
    Primary,
    Compact,
    Rebind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActualHostWorkflow {
    RootDelivery,
    SendNow,
    Fork,
    Rebind,
    Compact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActualHostBoundary {
    Prepared,
    RuntimeReturned,
    ProductPublished,
    ProductCommitted,
    TerminalReady,
}

pub enum ActualHostAction {
    Prompt(String),
    PromptOnce(String),
    SendQueuedNow { id: String, revision: u64 },
    Fork,
    Compact,
    Quit,
}

pub struct ActualHostInjection {
    runtime:
        Arc<dyn Fn(InjectedRuntimeRole) -> Result<SessionRuntimeBinding, String> + Send + Sync>,
    rebind_target: SessionTargetBinding,
    observe: Arc<dyn Fn(ActualHostWorkflow, ActualHostBoundary) + Send + Sync>,
}

impl ActualHostInjection {
    pub fn new(
        runtime: Arc<
            dyn Fn(InjectedRuntimeRole) -> Result<SessionRuntimeBinding, String> + Send + Sync,
        >,
        rebind_target: SessionTargetBinding,
        observe: Arc<dyn Fn(ActualHostWorkflow, ActualHostBoundary) + Send + Sync>,
    ) -> Self {
        Self {
            runtime,
            rebind_target,
            observe,
        }
    }
}

pub fn run_actual_host(
    home: PathBuf,
    product_session_id: &str,
    injection: ActualHostInjection,
    actions: Vec<ActualHostAction>,
) -> Result<(), String> {
    let paths = PlakPaths::explicit(home);
    let session = product_sessions::load_session(&paths, product_session_id)?;
    let cwd = session.workspace_root.clone();
    let runtime = Arc::clone(&injection.runtime);
    let observe = Arc::clone(&injection.observe);
    let internal = Arc::new(TestHostInjection {
        runtime: Arc::new(move |role| {
            runtime(match role {
                TestRuntimeRole::Primary => InjectedRuntimeRole::Primary,
                TestRuntimeRole::Compact => InjectedRuntimeRole::Compact,
                TestRuntimeRole::Rebind => InjectedRuntimeRole::Rebind,
            })
        }),
        rebind_target: injection.rebind_target,
        observe: Arc::new(move |workflow, boundary| {
            observe(
                match workflow {
                    TestHostWorkflow::RootDelivery => ActualHostWorkflow::RootDelivery,
                    TestHostWorkflow::SendNow => ActualHostWorkflow::SendNow,
                    TestHostWorkflow::Fork => ActualHostWorkflow::Fork,
                    TestHostWorkflow::Rebind => ActualHostWorkflow::Rebind,
                    TestHostWorkflow::Compact => ActualHostWorkflow::Compact,
                    TestHostWorkflow::SourceRelease => return,
                },
                match boundary {
                    TestHostBoundary::Prepared => ActualHostBoundary::Prepared,
                    TestHostBoundary::RuntimeReturned => ActualHostBoundary::RuntimeReturned,
                    TestHostBoundary::ProductPublished => ActualHostBoundary::ProductPublished,
                    TestHostBoundary::ProductCommitted => ActualHostBoundary::ProductCommitted,
                    TestHostBoundary::TerminalReady => ActualHostBoundary::TerminalReady,
                },
            );
        }),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let attached_one_shot = actions
        .iter()
        .any(|action| matches!(action, ActualHostAction::PromptOnce(_)));
    let actions = actions
        .into_iter()
        .map(|action| match action {
            ActualHostAction::Prompt(text) | ActualHostAction::PromptOnce(text) => {
                crate::surface_protocol::UserAction::Prompt(text.into())
            }
            ActualHostAction::SendQueuedNow { id, revision } => {
                crate::surface_protocol::UserAction::SendQueuedNow { id, revision }
            }
            ActualHostAction::Fork => crate::surface_protocol::UserAction::ForkSession {
                account: None,
                compact: false,
            },
            ActualHostAction::Compact => crate::surface_protocol::UserAction::Compact,
            ActualHostAction::Quit => crate::surface_protocol::UserAction::Quit,
        })
        .collect();
    product_host::run_injected_host(
        HostConfig {
            surface: Some(crate::shell::settings::ProductSurface::boxed(&paths)),
            auto_continue: true,
            topology_admission: None,
            state_roots: crate::shell::host::product_state_roots(&paths),
            home: paths.session_home(),
            store: Arc::new(crate::store::session_records::SqliteSessionStore::new(
                paths.clone(),
            )),
            topology_enabled: crate::product_config::SessionPolicy::load(&paths)
                .unwrap()
                .topology_enabled,
            product_home: paths.home().to_path_buf(),
            session,
            target: product_host::HostTargetAuthority::Injected {
                provider: NativeProvider::Codex,
            },
            model_id: "offline-loopback-v1".into(),
            effort: Some(ReasoningEffort::Medium),
            effort_explicit: true,
            compaction_enabled: true,
            max_model_rounds: crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            root_template: None,
            attached_one_shot,
            interactive: true,
            cwd,
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
            resume_publication: None,
            test_injection: None,
            ready_path: product_host::ReadyPathTiming::default(),
            seed_history: None,
            source_releases: product_host::SourceReleases::default(),
        },
        internal,
        actions,
    )
    .map_err(|failure| failure.message().to_owned())
    .and_then(|observations| {
        let lost_jobs = observations
            .iter()
            .filter_map(|event| match event {
                crate::surface_protocol::HostEvent::JobTerminal { job_id, status, .. }
                    if status == "outcome_unknown" =>
                {
                    Some(job_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&lost_jobs).map_err(|error| error.to_string())?;
        std::fs::write(paths.home().join("actual-host-lost-jobs.json"), bytes)
            .map_err(|error| error.to_string())
    })
}

impl ProductJournal {
    pub fn create(home: PathBuf, workspace: PathBuf) -> Result<Self, String> {
        let paths = PlakPaths::explicit(home);
        let record = SessionRecord::new(workspace);
        let mut journal = Self { paths, record };
        journal.persist()?;
        Ok(journal)
    }

    pub fn open(home: PathBuf, product_session_id: &str) -> Result<Self, String> {
        let paths = PlakPaths::explicit(home);
        let record = product_sessions::load_session(&paths, product_session_id)?;
        Ok(Self { paths, record })
    }

    #[must_use]
    pub fn product_session_id(&self) -> &str {
        &self.record.id
    }

    pub fn bind_locator(&mut self, locator: SessionLocator) -> Result<(), String> {
        self.record.bind_plakous_locator(locator)?;
        self.persist()
    }

    pub fn set_product_binding(
        &mut self,
        account: &str,
        model: &str,
        effort: &str,
    ) -> Result<(), String> {
        self.record.account = Some(account.to_owned());
        self.record.model = Some(model.to_owned());
        self.record.effort = Some(effort.to_owned());
        self.persist()
    }

    pub fn enqueue(&mut self, text: impl Into<String>) -> Result<(String, u64), String> {
        let item = QueueItem::new(text);
        let identity = (item.id.clone(), item.revision);
        self.record.pending_queue.push(item);
        self.persist()?;
        Ok(identity)
    }

    #[must_use]
    pub fn snapshot(&self) -> JournalSnapshot {
        JournalSnapshot {
            product_session_id: self.record.id.clone(),
            locator: self.record.plakous_locator.map(|value| value.to_string()),
            queue_items: self.record.pending_queue.len(),
            queue_paused: self.record.queue_drain_state == QueueDrainState::RestoredPaused,
            source_product_session_id: self.record.source_product_session_id.clone(),
            source_workflow_operation_id: self.record.source_workflow_operation_id.clone(),
        }
    }

    fn persist(&mut self) -> Result<(), String> {
        product_sessions::save_session(&self.paths, &mut self.record)
    }
}

/// Scale an interactive latency budget for the machine the suite is running on.
///
/// The PTY e2e tests assert wall-clock budgets, and `nextest` runs them
/// alongside every other test in the workspace. On a loaded box those budgets
/// are flaky by construction: the process is not slow, it is descheduled.
/// Two multipliers apply, and they compose:
///
/// * `PLAK_TEST_TIME_SCALE` — an explicit float the operator or CI sets. `1`
///   (the default) means "assert the interactive budget as written". A machine
///   that cannot hold it sets `2`, `4`, … rather than deleting the assertion.
/// * Load — the 1-minute run-queue length over the CPU count, read from
///   `/proc/loadavg` where it exists. An idle box asserts the budget as
///   written; a box saturated one-job-per-CPU gets 2x; load ~100 on this
///   128-core box gets ~1.8x. Contention buys budget instead of a failure that
///   says nothing about the product.
///
/// These budgets are a smoke check that nothing regressed by an order of
/// magnitude, not the §11 benchmark; the named benchmark owns the real number.
#[must_use]
pub fn latency_budget(budget: Duration) -> Duration {
    budget.mul_f64(time_scale() * load_multiplier())
}

fn time_scale() -> f64 {
    parse_time_scale(std::env::var("PLAK_TEST_TIME_SCALE").ok().as_deref())
}

/// A scale below 1, a non-number, or an absent variable all leave the budget
/// exactly as written at the call site.
fn parse_time_scale(value: Option<&str>) -> f64 {
    value
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 1.0)
        .unwrap_or(1.0)
}

fn load_multiplier() -> f64 {
    let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") else {
        return 1.0;
    };
    let Some(one_minute) = loadavg
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
    else {
        return 1.0;
    };
    let cpus = std::thread::available_parallelism().map_or(1.0, |count| {
        f64::from(u32::try_from(count.get()).unwrap_or(u32::MAX))
    });
    // Idle is 1x; one runnable job per CPU is 2x; bounded so a runaway
    // loadavg cannot turn a hung test into a 45-minute wait.
    (1.0 + one_minute / cpus).min(8.0)
}

#[cfg(test)]
mod latency_budget_tests {
    use super::{latency_budget, load_multiplier, parse_time_scale};
    use std::time::Duration;

    /// The PTY budgets must scale rather than flake. Both multipliers are
    /// bounded in both directions, so a budget can never shrink below the one
    /// written at the call site and can never grow without bound.
    #[test]
    fn the_budget_only_ever_grows_and_stays_bounded() {
        let base = Duration::from_secs(2);
        let scaled = latency_budget(base);
        assert!(scaled >= base, "a budget never shrinks: {scaled:?}");
        assert!(
            scaled <= base * 8 * 8,
            "both multipliers stay bounded: {scaled:?}"
        );

        let load = load_multiplier();
        assert!(
            (1.0..=8.0).contains(&load),
            "the load multiplier stays in range: {load}"
        );
    }

    /// An absent, malformed, or shrinking `PLAK_TEST_TIME_SCALE` is ignored:
    /// the variable can only ever buy a test more time, never less.
    #[test]
    fn only_a_finite_scale_of_at_least_one_is_honoured() {
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("-3"),
            Some("0.5"),
            Some("not-a-number"),
            Some("NaN"),
            Some("inf"),
        ] {
            assert!(
                (parse_time_scale(value) - 1.0).abs() < f64::EPSILON,
                "{value:?} must not change the budget"
            );
        }
        assert!((parse_time_scale(Some("4")) - 4.0).abs() < f64::EPSILON);
        assert!((parse_time_scale(Some(" 2.5 ")) - 2.5).abs() < f64::EPSILON);
    }
}
