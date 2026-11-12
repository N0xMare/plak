//! Ephemeral entry authority over the fixed execution-side Changes contract.

use super::{HostPrincipal, HostRequestContext, PAGE, WorkspaceTarget, b64, backend, failure};
use plakd_wire_types::console::{
    ChangeEntry, ChangesStatus, HostRequest, HostResponse, ReadRefusal, WorkspaceReadFailure,
};
use plakous::tools::{OperationContext, changes as execution};
use plakous_host_protocol::wire::RequestRefusal;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_millis(super::LIMITS.changes_token_ttl_ms);
#[derive(Default)]
pub(super) struct State {
    results: BTreeMap<(HostPrincipal, WorkspaceTarget), Snapshot>,
    pending: BTreeSet<(HostPrincipal, WorkspaceTarget)>,
}
struct Snapshot {
    generation: u64,
    instance: String,
    at: Instant,
    status: execution::ChangesStatus,
    entries: BTreeMap<String, usize>,
}

impl State {
    fn reserve(
        &mut self,
        context: &HostRequestContext,
        target: &WorkspaceTarget,
    ) -> Result<(), RequestRefusal> {
        self.results.retain(|_, result| result.at.elapsed() < TTL);
        let key = (context.principal.clone(), target.clone());
        if !self.results.contains_key(&key)
            && (self.results.len() + self.pending.len() >= 64
                || self
                    .results
                    .keys()
                    .chain(self.pending.iter())
                    .filter(|(principal, _)| principal == &context.principal)
                    .count()
                    >= super::LIMITS.changes_targets_per_principal as usize)
        {
            return Err(RequestRefusal::RateLimited {
                retry_after_ms: super::LIMITS.changes_token_ttl_ms,
            });
        }
        if !self.results.contains_key(&key) {
            self.pending.insert(key);
        }
        Ok(())
    }

    fn publish(
        &mut self,
        context: &HostRequestContext,
        target: WorkspaceTarget,
        instance: String,
        status: execution::ChangesStatus,
    ) -> Result<HostResponse, ReadRefusal> {
        if status.entries.len() as u64 > super::LIMITS.changes_entries {
            return Err(failure(WorkspaceReadFailure::TooLarge));
        }
        let encoded = serde_json::to_vec(&status).map_err(|_| failure(WorkspaceReadFailure::Io))?;
        let revision = plakous::model::sha256_hex(&[&encoded]);
        let mut snapshot = Snapshot {
            generation: context.host_generation,
            instance,
            at: Instant::now(),
            status,
            entries: BTreeMap::new(),
        };
        let mut entries = Vec::new();
        let mut bytes = 512_usize
            + serde_json::to_vec(&snapshot.status.base_label)
                .map_err(|_| failure(WorkspaceReadFailure::Io))?
                .len();
        let mut truncated = false;
        for (index, row) in snapshot.status.entries.iter().enumerate() {
            let token = uuid::Uuid::new_v4().to_string();
            let entry = ChangeEntry {
                entry: token.clone(),
                path: row.path.clone(),
                index: row.state.get(..1).unwrap_or("?").into(),
                worktree: row.state.get(1..2).unwrap_or("?").into(),
                untracked: row.state == "??",
            };
            let size = serde_json::to_vec(&entry)
                .map_err(|_| failure(WorkspaceReadFailure::Io))?
                .len()
                + 1;
            if bytes.saturating_add(size) > PAGE as usize {
                truncated = true;
                break;
            }
            bytes += size;
            snapshot.entries.insert(token, index);
            entries.push(entry);
        }
        let response = HostResponse::ChangesStatus {
            status: ChangesStatus {
                revision,
                base: snapshot.status.base_label.clone(),
                git_view: snapshot.status.repository.git_view,
                entries,
                truncated,
            },
        };
        let key = (context.principal.clone(), target);
        self.pending.remove(&key);
        self.results.insert(key, snapshot);
        Ok(response)
    }

    fn diff(
        &self,
        context: &HostRequestContext,
        target: &WorkspaceTarget,
        instance: &str,
        entry: &str,
    ) -> Result<execution::ChangesRequest, ReadRefusal> {
        let conflict = || failure(WorkspaceReadFailure::ChangesConflict);
        let snapshot = self
            .results
            .get(&(context.principal.clone(), target.clone()))
            .ok_or_else(conflict)?;
        if snapshot.at.elapsed() >= TTL
            || snapshot.generation != context.host_generation
            || snapshot.instance != instance
        {
            return Err(conflict());
        }
        let index = snapshot.entries.get(entry).ok_or_else(conflict)?;
        let row = snapshot
            .status
            .entries
            .get(*index)
            .ok_or_else(conflict)?
            .clone();
        execution::ChangesRequest::diff(
            snapshot.status.repository.clone(),
            snapshot.status.observation.clone(),
            row,
        )
        .map_err(refusal)
    }
}

pub(super) struct Reservation {
    console: Arc<super::Console>,
    principal: HostPrincipal,
    target: WorkspaceTarget,
}
impl Reservation {
    pub(super) fn admit(
        console: Arc<super::Console>,
        context: &HostRequestContext,
        target: &WorkspaceTarget,
    ) -> Result<Self, RequestRefusal> {
        console
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .changes
            .reserve(context, target)?;
        Ok(Self {
            console,
            principal: context.principal.clone(),
            target: target.clone(),
        })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.console
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .changes
            .pending
            .remove(&(self.principal.clone(), self.target.clone()));
    }
}

impl super::Console {
    pub(super) async fn changes(
        &self,
        context: &HostRequestContext,
        request: HostRequest,
        workspace: super::resolve::Workspace,
        control: OperationContext,
    ) -> Result<HostResponse, ReadRefusal> {
        let target = workspace.target.clone();
        let query = match request {
            HostRequest::ChangesStatus { .. } => execution::ChangesRequest::status(),
            HostRequest::ChangesDiff { entry, .. } => self
                .files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .changes
                .diff(context, &target, &workspace.identity, &entry)?,
            _ => unreachable!(),
        };
        match execution::workspace_changes(
            &workspace.environment,
            query,
            control,
            plakous_hosted::occupancy_vcs::git_cli_enabled(),
        )
        .await
        .map_err(backend)?
        {
            execution::ChangesAnswer::Status(status) => self
                .files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .changes
                .publish(context, target, workspace.identity, status),
            execution::ChangesAnswer::Diff(diff) => {
                let (bytes, binary) = match diff {
                    execution::ChangesDiff::Patch { text } => (text, false),
                    execution::ChangesDiff::Binary { numstat } => (numstat, true),
                };
                Ok(HostResponse::ChangesDiff {
                    bytes_b64: b64(bytes.as_bytes()),
                    binary,
                    truncated: false,
                })
            }
            execution::ChangesAnswer::Refused(reason) => Err(refusal(reason)),
        }
    }
}

fn refusal(reason: execution::ChangesRefusal) -> ReadRefusal {
    use execution::ChangesRefusal as R;
    failure(match reason {
        R::NotARepository => WorkspaceReadFailure::NotARepository,
        R::VcsUnavailable => WorkspaceReadFailure::VcsUnavailable,
        R::VcsUnsupported => WorkspaceReadFailure::VcsUnsupported,
        R::ProtectedPath | R::UnsafeRepository => WorkspaceReadFailure::ProtectedPath,
        R::InvalidRequest => WorkspaceReadFailure::InvalidPath,
        R::Conflict => WorkspaceReadFailure::ChangesConflict,
        R::TooLarge | R::OutputLimit => WorkspaceReadFailure::TooLarge,
        R::Deadline | R::Cancelled => WorkspaceReadFailure::Deadline,
        R::Io => WorkspaceReadFailure::Io,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "ephemeral entry authority fixtures")]
mod tests {
    use super::*;
    #[cfg(feature = "git-cli")]
    use plakous::tools::runner::{
        RunnerCall, RunnerCallFuture, RunnerClass, RunnerFailure, RunnerLink, RunnerVerb,
    };
    fn context() -> HostRequestContext {
        HostRequestContext {
            principal: HostPrincipal::Socket(1),
            connection_id: 1,
            host_generation: 1,
            request_id: 1,
        }
    }
    fn target(session: &str) -> WorkspaceTarget {
        WorkspaceTarget {
            session: session.into(),
            node: None,
        }
    }
    fn status() -> execution::ChangesStatus {
        execution::ChangesStatus {
            repository: execution::ChangesRepository {
                workspace: "/workspace".into(),
                toplevel: "/workspace".into(),
                git_dir: "/workspace/.git".into(),
                common_dir: "/workspace/.git".into(),
                identity: "a".repeat(64),
                git_view: false,
            },
            observation: execution::ChangesObservation {
                head: Some("b".repeat(40)),
                index: "c".repeat(64),
            },
            base_label: "work tree against HEAD".into(),
            entries: vec![execution::ChangesEntry {
                path: "data".into(),
                state: ".M".into(),
                head_object: Some("d".repeat(40)),
                index_object: Some("d".repeat(40)),
                content: execution::ChangesContent::File {
                    size: 3,
                    digest: "e".repeat(64),
                },
            }],
        }
    }
    fn token(response: HostResponse) -> String {
        let HostResponse::ChangesStatus { status } = response else {
            panic!("status")
        };
        status.entries[0].entry.clone()
    }
    #[test]
    fn tokens_bind_principal_target_instance_generation_and_status_revision() {
        let mut state = State::default();
        let selected = target("saved");
        let first = token(
            state
                .publish(&context(), selected.clone(), "instance".into(), status())
                .unwrap(),
        );
        assert!(
            state
                .diff(&context(), &selected, "instance", &first)
                .is_ok()
        );
        let mut other = context();
        other.principal = HostPrincipal::Socket(2);
        assert!(state.diff(&other, &selected, "instance", &first).is_err());
        other = context();
        other.host_generation += 1;
        assert!(state.diff(&other, &selected, "instance", &first).is_err());
        assert!(
            state
                .diff(&context(), &target("other"), "instance", &first)
                .is_err()
        );
        assert!(
            state
                .diff(&context(), &selected, "new-instance", &first)
                .is_err()
        );
        let mut changed = status();
        changed.observation.head = Some("f".repeat(40));
        let second = token(
            state
                .publish(&context(), selected.clone(), "instance".into(), changed)
                .unwrap(),
        );
        assert_ne!(first, second);
        assert!(
            state
                .diff(&context(), &selected, "instance", &first)
                .is_err()
        );
        state
            .results
            .get_mut(&(context().principal, selected.clone()))
            .unwrap()
            .at -= TTL;
        assert!(
            state
                .diff(&context(), &selected, "instance", &second)
                .is_err()
        );
        assert!(
            State::default()
                .diff(&context(), &selected, "instance", &second)
                .is_err()
        );
    }
    #[test]
    fn result_ownership_is_bounded_before_work_and_truncation_is_not_clean() {
        let mut state = State::default();
        for n in 0..4 {
            state
                .publish(
                    &context(),
                    target(&n.to_string()),
                    "instance".into(),
                    status(),
                )
                .unwrap();
        }
        assert!(state.reserve(&context(), &target("fifth")).is_err());
        assert!(state.reserve(&context(), &target("0")).is_ok());
        let mut huge = status();
        huge.entries.resize(2_001, huge.entries[0].clone());
        assert_eq!(
            state.publish(&context(), target("huge"), "instance".into(), huge),
            Err(failure(WorkspaceReadFailure::TooLarge))
        );
        let mut full = status();
        full.entries.resize(2_000, full.entries[0].clone());
        let response = state
            .publish(&context(), target("full"), "instance".into(), full)
            .unwrap();
        let HostResponse::ChangesStatus { status } = response else {
            panic!("status")
        };
        assert_eq!(status.entries.len(), 2_000);
        assert!(!status.truncated);
        assert!(serde_json::to_vec(&status).unwrap().len() <= PAGE as usize);
        assert_eq!(
            state.results[&(context().principal, target("full"))]
                .status
                .entries
                .len(),
            2_000
        );
    }

    #[test]
    fn token_overhead_truncates_the_client_status_without_claiming_clean() {
        let mut observed = status();
        observed.entries = (0..2_000)
            .map(|n| execution::ChangesEntry {
                path: format!("{n:030}"),
                state: ".D".into(),
                head_object: None,
                index_object: None,
                content: execution::ChangesContent::Missing,
            })
            .collect();
        let response = State::default()
            .publish(&context(), target("saved"), "instance".into(), observed)
            .unwrap();
        assert!(serde_json::to_vec(&response).unwrap().len() <= PAGE as usize);
        let HostResponse::ChangesStatus { status } = response else {
            panic!("status")
        };
        assert!(status.truncated && !status.entries.is_empty() && status.entries.len() < 2_000);
    }

    #[test]
    fn concurrent_status_reservations_count_toward_the_process_result_bound() {
        let mut state = State::default();
        for index in 0..4 {
            assert!(
                state
                    .reserve(&context(), &target(&index.to_string()))
                    .is_ok()
            );
        }
        assert!(state.reserve(&context(), &target("fifth")).is_err());
        let mut state = State::default();
        for uid in 0..64 {
            let mut context = context();
            context.principal = HostPrincipal::Socket(uid);
            assert!(state.reserve(&context, &target("saved")).is_ok());
        }
        let mut next = context();
        next.principal = HostPrincipal::Socket(64);
        assert!(state.reserve(&next, &target("saved")).is_err());
    }

    #[test]
    fn status_revision_binds_repository_head_index_and_worktree_content() {
        let mut previous = None;
        for input in 0..5 {
            let mut observed = status();
            match input {
                1 => observed.repository.identity = "f".repeat(64),
                2 => observed.observation.head = Some("f".repeat(40)),
                3 => observed.observation.index = "f".repeat(64),
                4 => observed.entries[0].content = execution::ChangesContent::Missing,
                _ => {}
            }
            let HostResponse::ChangesStatus { status } = State::default()
                .publish(&context(), target("saved"), "instance".into(), observed)
                .unwrap()
            else {
                panic!("status")
            };
            if let Some(previous) = previous {
                assert_ne!(status.revision, previous);
            }
            previous = Some(status.revision);
        }
    }

    #[tokio::test]
    async fn reservations_release_on_drop_and_expired_results_free_capacity() {
        let root = tempfile::tempdir().unwrap();
        let console = super::super::tests::console(root.path());
        let selected = target("saved");
        let reservation = Reservation::admit(Arc::clone(&console), &context(), &selected).unwrap();
        assert_eq!(console.files.lock().unwrap().changes.pending.len(), 1);
        drop(reservation);
        assert_eq!(console.files.lock().unwrap().changes.pending.len(), 0);
        let mut state = State::default();
        for n in 0..4 {
            state
                .publish(
                    &context(),
                    target(&n.to_string()),
                    "instance".into(),
                    status(),
                )
                .unwrap();
        }
        for snapshot in state.results.values_mut() {
            snapshot.at -= TTL;
        }
        assert!(state.reserve(&context(), &target("new")).is_ok());
        assert_eq!(state.results.len(), 0);
    }

    #[cfg(feature = "git-cli")]
    fn saved_session(
        paths: &crate::home::paths::PlakPaths,
        root: &std::path::Path,
    ) -> (
        crate::shell::services::Services,
        crate::store::sessions::SessionRecord,
        crate::host::sessions::ProductSessions,
    ) {
        let services = crate::shell::services::Services::for_home(paths.clone());
        services
            .runners()
            .configure(plakous::runner::ConfiguredRunner {
                runner_id: "local".into(),
                names: vec!["local".into()],
            });
        services
            .runners()
            .ready_local(
                Arc::new(
                    plakous::runner::WorkspaceCatalog::local(None, &[], paths.home()).unwrap(),
                ),
                0,
            )
            .unwrap();
        let mut record = crate::store::sessions::SessionRecord::new(root.to_owned());
        services
            .session_records()
            .unwrap()
            .save(&mut record)
            .unwrap();
        let sessions = crate::host::sessions::ProductSessions::new(
            &services,
            None,
            Some(tokio::runtime::Handle::current()),
        )
        .unwrap();
        (services, record, sessions)
    }

    #[cfg(feature = "git-cli")]
    fn git(root: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "git fixture failed");
    }

    #[cfg(feature = "git-cli")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saved_status_and_diff_need_no_attachment_runtime_or_agent_wake() {
        use plakous_hosted::host_service::wire::HostSessions;
        let (_home, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let work = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(work.path()).unwrap();
        git(&root, &["init", "--quiet"]);
        std::fs::write(root.join("file"), "before\n").unwrap();
        git(&root, &["add", "file"]);
        git(&root, &["commit", "--quiet", "-m", "fixture"]);
        std::fs::write(root.join("file"), "after\n").unwrap();
        let (services, record, sessions) = saved_session(&paths, &root);
        let revision = record.store_revision;
        let selected = WorkspaceTarget {
            session: record.id.clone(),
            node: None,
        };
        let HostResponse::ChangesStatus { status: first } = sessions
            .host_request(
                context(),
                HostRequest::ChangesStatus {
                    target: selected.clone(),
                },
            )
            .await
            .unwrap()
        else {
            panic!("status")
        };
        assert!(!first.truncated && first.entries.len() == 1);
        let entry = first.entries[0].entry.clone();
        let diff = || HostRequest::ChangesDiff {
            target: selected.clone(),
            entry: entry.clone(),
        };
        let HostResponse::ChangesDiff {
            bytes_b64,
            binary,
            truncated,
        } = sessions.host_request(context(), diff()).await.unwrap()
        else {
            panic!("diff")
        };
        assert!(!binary && !truncated);
        let patch =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, bytes_b64).unwrap();
        assert!(
            String::from_utf8(patch)
                .unwrap()
                .contains("-before\n+after")
        );
        let mut other = context();
        other.principal = HostPrincipal::Socket(2);
        assert_eq!(
            sessions.host_request(other, diff()).await.unwrap(),
            super::super::refused(failure(WorkspaceReadFailure::ChangesConflict))
        );
        let explicit = WorkspaceTarget {
            session: record.id.clone(),
            node: Some(record.node.agent_id.clone()),
        };
        let HostResponse::ChangesStatus { status: second } = sessions
            .host_request(context(), HostRequest::ChangesStatus { target: explicit })
            .await
            .unwrap()
        else {
            panic!("status")
        };
        assert_eq!(first.revision, second.revision);
        assert_eq!(
            sessions.host_request(context(), diff()).await.unwrap(),
            super::super::refused(failure(WorkspaceReadFailure::ChangesConflict))
        );
        std::fs::write(root.join("file"), "newer\n").unwrap();
        assert_eq!(
            sessions
                .host_request(
                    context(),
                    HostRequest::ChangesDiff {
                        target: selected,
                        entry: second.entries[0].entry.clone(),
                    }
                )
                .await
                .unwrap(),
            super::super::refused(failure(WorkspaceReadFailure::ChangesConflict))
        );
        assert!(!paths.runtime_database().exists());
        assert_eq!(
            services
                .session_records()
                .unwrap()
                .load(&record.id)
                .unwrap()
                .store_revision,
            revision
        );
    }

    #[cfg(feature = "git-cli")]
    struct Endpoint {
        status: execution::ChangesStatus,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        patch: &'static str,
    }
    #[cfg(feature = "git-cli")]
    impl RunnerLink for Endpoint {
        fn call(&self, call: RunnerCall) -> RunnerCallFuture {
            assert!(matches!(call.class(), RunnerClass::Unkeyed));
            assert!(call.owner().is_none());
            let answer = if call.verb() == RunnerVerb::WorkspaceChanges {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let deadline = call.budget().unwrap();
                let millis =
                    deadline.seconds() * 1000 + u64::from(deadline.nanoseconds()) / 1_000_000;
                assert!(millis.saturating_sub(plakous::model::now_unix_millis()) <= 5_000);
                let request = &call.payload()["request"];
                let answer = if request["query"] == "status" {
                    execution::ChangesAnswer::Status(self.status.clone())
                } else {
                    assert_eq!(
                        request["repository"]["identity"],
                        self.status.repository.identity
                    );
                    assert_eq!(
                        request["observation"]["index"],
                        self.status.observation.index
                    );
                    execution::ChangesAnswer::Diff(execution::ChangesDiff::Patch {
                        text: self.patch.into(),
                    })
                };
                Ok(serde_json::to_value(answer).unwrap())
            } else {
                match call.payload()["op"].as_str() {
                    Some("canonicalize") => Ok(serde_json::json!({"path":call.payload()["path"]})),
                    Some("filesystem_identity") => Ok(serde_json::json!({"device":1,"inode":2})),
                    _ => Err(RunnerFailure::Unsupported),
                }
            };
            Box::pin(std::future::ready(answer))
        }
    }
    #[cfg(feature = "git-cli")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn changes_use_the_selected_runner_and_stale_instances_never_dispatch() {
        use plakous::tools::runner::{RunnerBackend, runner_descriptor, runner_environment};
        use plakous::tools::{ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let console = super::super::tests::console(root.path());
        for (id, patch) in [("first", "first diff\n"), ("second", "second diff\n")] {
            let mut observed = status();
            observed.repository.workspace = canonical.clone();
            observed.repository.toplevel = canonical.clone();
            observed.repository.git_dir = canonical.join(".git");
            observed.repository.common_dir = canonical.join(".git");
            let calls = Arc::new(AtomicUsize::new(0));
            let direct = super::super::tests::workspace(&canonical);
            let descriptor =
                runner_descriptor(&direct.environment, RunnerId::new(id).unwrap()).unwrap();
            let backend = Arc::new(
                RunnerBackend::new(
                    descriptor,
                    Arc::new(Endpoint {
                        status: observed,
                        calls: Arc::clone(&calls),
                        patch,
                    }),
                )
                .unwrap(),
            );
            let scope = ExecutionEnvironmentScope::Run(
                plakous::model::RunId::try_from(uuid::Uuid::new_v4()).unwrap(),
            );
            let environment = runner_environment(
                scope,
                &canonical,
                &canonical,
                ExecutionEnvironmentConfig::default(),
                backend,
            )
            .unwrap();
            let placed = || super::super::resolve::Workspace {
                environment: environment.clone(),
                identity: id.into(),
                placement: None,
                target: target("saved"),
            };
            let entry = token(
                console
                    .inspect(
                        &context(),
                        HostRequest::ChangesStatus {
                            target: target("saved"),
                        },
                        placed(),
                    )
                    .await
                    .unwrap(),
            );
            let diff = || HostRequest::ChangesDiff {
                target: target("saved"),
                entry: entry.clone(),
            };
            let HostResponse::ChangesDiff { bytes_b64, .. } =
                console.inspect(&context(), diff(), placed()).await.unwrap()
            else {
                panic!("diff")
            };
            assert_eq!(
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, bytes_b64)
                    .unwrap(),
                patch.as_bytes()
            );
            let mut restarted = placed();
            restarted.identity.push_str("-restarted");
            assert_eq!(
                console
                    .inspect(&context(), diff(), restarted)
                    .await
                    .unwrap(),
                super::super::refused(failure(WorkspaceReadFailure::ChangesConflict))
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }
    }
}
