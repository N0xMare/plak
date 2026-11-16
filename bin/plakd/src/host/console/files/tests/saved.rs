use super::*;
use plakous::tools::runner::{RunnerBackend, RunnerCall, RunnerCallFuture, RunnerLink, RunnerVerb};
use plakous_hosted::host_service::wire::HostSessions;

#[tokio::test]
async fn failed_page_retry_keeps_cursor_and_capacity_is_checked_before_io() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("data");
    std::fs::write(&path, "abcdef").unwrap();
    let console = console(root.path());
    let mut tokens = Vec::new();
    for _ in 0..64 {
        let HostResponse::FilesRead { content } = console
            .inspect(&context(), request("data", None, 1), workspace(root.path()))
            .await
            .unwrap()
        else {
            panic!("content")
        };
        tokens.push(content.next.unwrap());
    }
    let refusal = console
        .inspect(
            &context(),
            request("missing", None, 1),
            workspace(root.path()),
        )
        .await
        .unwrap();
    assert!(matches!(
        refusal,
        HostResponse::Refused {
            reason: ReadRefusal::ValidationFailed { .. }
        }
    ));
    std::fs::remove_file(&path).unwrap();
    let token = tokens.pop().unwrap();
    assert_eq!(
        console
            .inspect(
                &context(),
                request("data", Some(token.clone()), 1),
                workspace(root.path())
            )
            .await
            .unwrap(),
        refused(failure(WorkspaceReadFailure::NotFound))
    );
    std::fs::write(&path, "abcdef").unwrap();
    assert!(matches!(
        console
            .inspect(
                &context(),
                request("data", Some(token.clone()), 1),
                workspace(root.path())
            )
            .await
            .unwrap(),
        HostResponse::FilesRead { .. }
    ));
    assert_eq!(
        console
            .inspect(
                &context(),
                request("data", Some(token), 1),
                workspace(root.path())
            )
            .await
            .unwrap(),
        refused(failure(WorkspaceReadFailure::InvalidCursor))
    );
}

#[tokio::test]
async fn invalid_utf8_rows_are_skipped_without_hiding_valid_list_or_search_results() {
    use std::os::unix::ffi::OsStringExt as _;
    let root = tempfile::tempdir().unwrap();
    let invalid = std::ffi::OsString::from_vec(vec![0xff]);
    assert!(
        RepositoryQuery::list(
            std::path::PathBuf::from(&invalid),
            None,
            1,
            query_limits(10).unwrap()
        )
        .is_err()
    );
    std::fs::write(root.path().join("valid"), "needle").unwrap();
    if !write_non_utf8(&root.path().join(invalid), "needle\nneedle") {
        return;
    }
    let console = console(root.path());
    let HostResponse::FilesList { entries, summary } = console
        .inspect(
            &context(),
            HostRequest::FilesList {
                target: target(),
                path: ".".into(),
            },
            workspace(root.path()),
        )
        .await
        .unwrap()
    else {
        panic!("list")
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "valid");
    assert_eq!(summary.skipped_entries, 1);
    let HostResponse::FilesSearch { matches, summary } = console
        .inspect(
            &context(),
            HostRequest::FilesSearch {
                target: target(),
                path: ".".into(),
                literal: "needle".into(),
                match_case: None,
            },
            workspace(root.path()),
        )
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "valid");
    assert_eq!(summary.skipped_entries, 1);
}

fn ready_local(services: &crate::shell::services::Services) {
    services.paths().unwrap().ensure_layout().unwrap();
    let registry = services.runners();
    registry.configure(plakous::runner::ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    registry
        .ready_local(
            Arc::new(
                plakous::runner::WorkspaceCatalog::local(
                    None,
                    &[],
                    services.paths().unwrap().home(),
                )
                .unwrap(),
            ),
            0,
        )
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_local_unrestricted_roots_still_protect_both_homes_in_both_directions() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("product");
    let state = root.path().join("xdg");
    let paths = crate::home::paths::PlakPaths::explicit(home.clone());
    let services =
        crate::shell::services::Services::for_home_with_state(paths.clone(), state.clone());
    ready_local(&services);
    let sessions = crate::host::sessions::ProductSessions::new(
        &services,
        None,
        Some(tokio::runtime::Handle::current()),
    )
    .unwrap();
    for candidate in [
        home.join("inner"),
        root.path().to_path_buf(),
        state.join("plak/inner"),
        state,
    ] {
        std::fs::create_dir_all(&candidate).unwrap();
        let mut record =
            crate::store::sessions::SessionRecord::new(std::fs::canonicalize(candidate).unwrap());
        services
            .session_records()
            .unwrap()
            .save(&mut record)
            .unwrap();
        let request = HostRequest::FilesList {
            target: WorkspaceTarget {
                session: record.id,
                node: None,
            },
            path: ".".into(),
        };
        assert_eq!(
            sessions.host_request(context(), request).await.unwrap(),
            refused(failure(WorkspaceReadFailure::ProtectedPath))
        );
    }
    assert!(!paths.runtime_database().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_audit_covers_capability_rate_and_success_without_content() {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("private-path"), "private-body").unwrap();
    let console = console(root.path());
    let paths = crate::home::paths::PlakPaths::explicit(root.path().join("product"));
    let services = crate::shell::services::Services::for_home(paths.clone());
    ready_local(&services);
    let tree = Arc::new(crate::store::tree::open_tree_store(&paths).unwrap());
    let mut record =
        crate::store::sessions::SessionRecord::new(std::fs::canonicalize(work.path()).unwrap());
    services
        .session_records()
        .unwrap()
        .save(&mut record)
        .unwrap();
    let target = WorkspaceTarget {
        session: record.id.clone(),
        node: None,
    };
    let requests = audit_requests(target);
    check_audit_requests(console, services, tree, requests, &record.id).await;
}

fn audit_requests(target: WorkspaceTarget) -> [HostRequest; 5] {
    [
        HostRequest::FilesList {
            target: target.clone(),
            path: "private-path".into(),
        },
        HostRequest::FilesRead {
            target: target.clone(),
            path: "private-path".into(),
            cursor: None,
            max_bytes: None,
        },
        HostRequest::FilesSearch {
            target: target.clone(),
            path: ".".into(),
            literal: "private-literal".into(),
            match_case: None,
        },
        HostRequest::ChangesStatus {
            target: target.clone(),
        },
        HostRequest::ChangesDiff {
            target,
            entry: "private-token".into(),
        },
    ]
}

async fn check_audit_requests(
    console: Arc<Console>,
    services: crate::shell::services::Services,
    tree: Arc<plakous_recursive_subagents_store::SqliteTreeStore>,
    requests: [HostRequest; 5],
    session: &str,
) {
    let mut web = context();
    web.principal = HostPrincipal::Web("verified".into());
    for request in &requests {
        assert!(
            console
                .workspace_request(web.clone(), request.clone(), services.clone(), tree.clone())
                .await
                .is_err()
        );
    }
    assert!(matches!(
        console
            .workspace_request(
                context(),
                requests[1].clone(),
                services.clone(),
                tree.clone()
            )
            .await
            .unwrap(),
        HostResponse::FilesRead { .. }
    ));
    let guards = (0..2)
        .map(|_| quota::Guard::admit(console.clone(), &context().principal, &requests[1]).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        console
            .workspace_request(context(), requests[1].clone(), services, tree)
            .await,
        Err(RequestRefusal::RateLimited { .. })
    ));
    drop(guards);
    let records = &console.audit.lock().unwrap().records;
    assert_eq!(records.len(), 7);
    for (row, action) in records.iter().take(5).zip([
        "files_list",
        "files_read",
        "files_search",
        "changes_status",
        "changes_diff",
    ]) {
        assert_eq!(row["action"], action);
        assert_eq!(row["category"], "refusal");
        assert_eq!(row["target"], session);
    }
    assert_eq!(records[5]["category"], "success");
    assert_eq!(records[6]["category"], "rate_limited");
    let encoded = serde_json::to_string(records).unwrap();
    for secret in [
        "private-path",
        "private-body",
        "private-literal",
        "private-token",
    ] {
        assert!(!encoded.contains(secret));
    }
}

struct Endpoint {
    stalled: bool,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}
impl RunnerLink for Endpoint {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        let dropped = self.dropped.clone();
        if self.stalled && call.verb() == RunnerVerb::ProbeWorkspace {
            return Box::pin(async move {
                struct Pending(Arc<std::sync::atomic::AtomicBool>);
                impl Drop for Pending {
                    fn drop(&mut self) {
                        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                let _pending = Pending(dropped);
                std::future::pending().await
            });
        }
        let answer = if call.verb() == RunnerVerb::ReadWorkspace {
            let bytes = b"remote-not-local";
            let content = plakous::tools::WorkspaceContentIdentity::from_bytes(bytes).unwrap();
            serde_json::json!({"content":{"byte_len":content.byte_len(),"digest":content.digest()},"offset":0,"bytes":plakous::tools::runner::hex(bytes),"next":null})
        } else {
            match call.payload()["op"].as_str() {
                Some("canonicalize") => serde_json::json!({"path":call.payload()["path"]}),
                Some("filesystem_identity") => serde_json::json!({"device":1,"inode":2}),
                _ => panic!("unexpected probe"),
            }
        };
        Box::pin(async move { Ok(answer) })
    }
}

async fn ready_remote(
    services: &crate::shell::services::Services,
    root: &std::path::Path,
    endpoint: Endpoint,
) -> tokio::task::JoinHandle<()> {
    use plakous::runner::protocol::{Bind, VersionTriple};
    use plakous::runner::{ClientWindows, RunnerClient, RunnerIdentity, RunnerService};
    let direct = workspace(root);
    let version = VersionTriple::current("fixture", "test");
    let service = RunnerService::new(
        direct.environment,
        RunnerIdentity {
            runner_id: plakous::tools::RunnerId::new("remote").unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
        },
    )
    .unwrap();
    let descriptor = service.hello().descriptor.clone();
    let (host, runner) = tokio::io::duplex(65536);
    let serving = tokio::spawn(async move { service.serve_connection(runner).await });
    let (client, _) = RunnerClient::connect(
        host,
        Bind {
            host_id: "host".into(),
            host_instance: "instance".into(),
            version,
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    let backend = Arc::new(RunnerBackend::new(descriptor, Arc::new(endpoint)).unwrap());
    let registry = services.runners();
    registry.configure(plakous::runner::ConfiguredRunner {
        runner_id: "remote".into(),
        names: vec!["remote".into()],
    });
    registry
        .ready(
            "remote",
            client.connection_revision(),
            plakous::runner::RunnerServices::new(backend, tokio::runtime::Handle::current()),
            plakous::runner::WorkspaceOptions {
                roots: Some(vec![root.to_str().unwrap().into()]),
                default_workspace: None,
                copies: false,
            },
            plakous::runner::WorkspaceDiscovery::default(),
            0,
        )
        .unwrap();
    serving
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_ready_remote_reads_remote_bytes_and_stalled_resolution_releases_slots() {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(work.path()).unwrap();
    std::fs::write(work.join("data"), "local-must-not-leak").unwrap();
    for stalled in [false, true] {
        let paths = crate::home::paths::PlakPaths::explicit(root.path().join(stalled.to_string()));
        let services = crate::shell::services::Services::for_home(paths.clone());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let serving = ready_remote(
            &services,
            &work,
            Endpoint {
                stalled,
                dropped: dropped.clone(),
            },
        )
        .await;
        let mut record = crate::store::sessions::SessionRecord::new(work.clone());
        record.runner = Some(plakous_hosted::sessions::RunnerDisplay {
            name: "remote".into(),
            runner_id: "remote".into(),
            workspace_root: work.clone(),
            workspace_cwd: work.clone(),
        });
        services
            .session_records()
            .unwrap()
            .save(&mut record)
            .unwrap();
        let request = HostRequest::FilesRead {
            target: WorkspaceTarget {
                session: record.id,
                node: None,
            },
            path: "data".into(),
            cursor: None,
            max_bytes: None,
        };
        let console = console(root.path());
        let tree = Arc::new(crate::store::tree::open_tree_store(&paths).unwrap());
        let started = std::time::Instant::now();
        let response = console
            .workspace_request(context(), request, services, tree)
            .await
            .unwrap();
        if stalled {
            assert_eq!(response, refused(failure(WorkspaceReadFailure::Deadline)));
            assert!(started.elapsed() < Duration::from_millis(LIMITS.file_deadline_ms + 500));
            assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        } else {
            let HostResponse::FilesRead { content } = response else {
                panic!("content")
            };
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(content.bytes_b64)
                    .unwrap(),
                b"remote-not-local"
            );
        }
        assert_eq!(console.files.lock().unwrap().quota.active_for_test(), 0);
        assert!(!paths.runtime_database().exists());
        serving.abort();
    }
}
