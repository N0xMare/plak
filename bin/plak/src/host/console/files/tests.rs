use super::*;
use plakd_wire_types::console::ConfiguredSwitches;

mod saved;

fn context() -> HostRequestContext {
    HostRequestContext {
        principal: HostPrincipal::Socket(1),
        connection_id: 2,
        host_generation: 3,
        request_id: 4,
    }
}
pub(super) fn target() -> WorkspaceTarget {
    WorkspaceTarget {
        session: "saved-session".into(),
        node: None,
    }
}
fn request(path: &str, cursor: Option<String>, bytes: u64) -> HostRequest {
    HostRequest::FilesRead {
        target: target(),
        path: path.into(),
        cursor,
        max_bytes: Some(bytes),
    }
}
pub(super) fn console(root: &std::path::Path) -> Arc<Console> {
    Console::new(
        crate::home::paths::PlakPaths::explicit(root.join("home")),
        root.join("state"),
        Some(tokio::runtime::Handle::current()),
        ConfiguredSwitches {
            web_accounts_read: false,
            web_config_read: false,
            web_credentials_write: false,
            web_config_write: false,
            web_files_read: false,
        },
    )
}
pub(super) fn workspace(path: &std::path::Path) -> resolve::Workspace {
    use plakous::tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };
    let scope = ExecutionEnvironmentScope::Run(
        plakous::model::RunId::try_from(uuid::Uuid::new_v4()).unwrap(),
    );
    resolve::Workspace {
        environment: ExecutionEnvironment::host(
            scope,
            path,
            path,
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap(),
        identity: path.to_str().unwrap().into(),
        placement: None,
        target: target(),
    }
}

#[tokio::test]
async fn exact_binary_pages_and_mutation_never_splice_versions() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("bytes");
    let bytes = [0xff, 0, 0xe2, 0x82, 0xac, 0x42];
    std::fs::write(&file, bytes).unwrap();
    let console = console(root.path());
    let first = console
        .inspect(
            &context(),
            request("./bytes", None, 3),
            workspace(root.path()),
        )
        .await
        .unwrap();
    let HostResponse::FilesRead { content: first } = first else {
        panic!("content")
    };
    assert_eq!(first.offset, 0);
    assert_eq!(first.size, 6);
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(first.bytes_b64)
            .unwrap(),
        bytes[..3]
    );
    let second = console
        .inspect(
            &context(),
            request("./bytes", first.next.clone(), 3),
            workspace(root.path()),
        )
        .await
        .unwrap();
    let HostResponse::FilesRead { content: second } = second else {
        panic!("content")
    };
    assert_eq!(second.revision, first.revision);
    assert_eq!(second.offset, 3);
    assert!(second.next.is_none());
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(second.bytes_b64)
            .unwrap(),
        bytes[3..]
    );
    let HostResponse::FilesRead { content: fresh } = console
        .inspect(
            &context(),
            request("bytes", None, 3),
            workspace(root.path()),
        )
        .await
        .unwrap()
    else {
        panic!("content")
    };
    std::fs::write(file, b"newest").unwrap();
    assert_eq!(
        console
            .inspect(
                &context(),
                request("bytes", fresh.next, 3),
                workspace(root.path())
            )
            .await
            .unwrap(),
        refused(failure(WorkspaceReadFailure::ContentChanged))
    );
}

#[test]
fn viewer_paths_and_product_and_state_overlap_are_closed_in_both_directions() {
    for path in ["/etc/passwd", "../escape", "a/../../escape", "a\0b"] {
        assert!(valid_path(path, true).is_err());
    }
    for path in [".git/config", "a/.GIT/objects", ".jj/repo"] {
        assert_eq!(
            valid_path(path, true),
            Err(failure(WorkspaceReadFailure::ProtectedPath))
        );
    }
    let root = tempfile::tempdir().unwrap();
    for name in ["product", "state/plak"] {
        let protected = root.path().join(name);
        std::fs::create_dir_all(protected.join("inner")).unwrap();
        for candidate in [
            root.path().to_owned(),
            protected.clone(),
            protected.join("inner"),
        ] {
            assert_eq!(
                resolve::protect(&candidate, std::slice::from_ref(&protected)),
                Err(failure(WorkspaceReadFailure::ProtectedPath))
            );
        }
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        assert!(resolve::protect(&work, &[protected]).is_ok());
    }
}

#[tokio::test]
async fn nonregular_symlinks_invalid_names_and_oversized_files_refuse_promptly() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("plain"), "safe").unwrap();
    symlink("plain", root.path().join("link")).unwrap();
    // rustix exposes mknodat only on Linux; mkfifo(1) is portable.
    let made = std::process::Command::new("mkfifo")
        .arg(root.path().join("fifo"))
        .status()
        .unwrap();
    assert!(made.success(), "mkfifo did not create the test fifo");
    let console = console(root.path());
    for path in ["link", "fifo"] {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            console.inspect(&context(), request(path, None, 16), workspace(root.path())),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(result, HostResponse::Refused { .. }));
    }
    assert_eq!(
        console
            .inspect(
                &context(),
                request("null", None, 16),
                workspace(std::path::Path::new("/dev"))
            )
            .await
            .unwrap(),
        refused(failure(WorkspaceReadFailure::NotRegularFile))
    );
    let file = std::fs::File::create(root.path().join("huge")).unwrap();
    file.set_len(INPUT + 1).unwrap();
    assert_eq!(
        console
            .inspect(&context(), request("huge", None, 1), workspace(root.path()))
            .await
            .unwrap(),
        refused(failure(WorkspaceReadFailure::FileTooLarge))
    );
    file.set_len(INPUT).unwrap();
    let HostResponse::FilesRead { content } = console
        .inspect(&context(), request("huge", None, 1), workspace(root.path()))
        .await
        .unwrap()
    else {
        panic!("ceiling is inclusive")
    };
    assert_eq!(content.size, INPUT);
    assert!(content.next.is_some());
    std::fs::write(
        root.path().join(std::ffi::OsString::from_vec(vec![0xff])),
        "data",
    )
    .unwrap();
    let result = console
        .inspect(
            &context(),
            HostRequest::FilesList {
                target: target(),
                path: ".".into(),
            },
            workspace(root.path()),
        )
        .await
        .unwrap();
    let HostResponse::FilesList { entries, summary } = result else {
        panic!("list")
    };
    assert!(entries.iter().any(|entry| entry.path == "plain"));
    assert!(summary.skipped_entries >= 1);
}

#[tokio::test]
async fn cursors_bind_principal_target_generation_path_and_execution_identity() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("data"), "abcdef").unwrap();
    let console = console(root.path());
    let HostResponse::FilesRead { content } = console
        .inspect(&context(), request("data", None, 1), workspace(root.path()))
        .await
        .unwrap()
    else {
        panic!("content")
    };
    let token = content.next.unwrap();
    let state = console.files.lock().unwrap();
    let identity = root.path().to_str().unwrap();
    let mut other = context();
    other.principal = HostPrincipal::Socket(9);
    assert!(
        state
            .cursors
            .get(&token, &other, &target(), identity, "data")
            .is_err()
    );
    other = context();
    other.host_generation += 1;
    assert!(
        state
            .cursors
            .get(&token, &other, &target(), identity, "data")
            .is_err()
    );
    let mut selected = target();
    selected.node = Some("child".into());
    assert!(
        state
            .cursors
            .get(&token, &context(), &selected, identity, "data")
            .is_err()
    );
    assert!(
        state
            .cursors
            .get(&token, &context(), &target(), "another-instance", "data")
            .is_err()
    );
    assert!(
        state
            .cursors
            .get(&token, &context(), &target(), identity, "other")
            .is_err()
    );
    assert!(
        state
            .cursors
            .get(&token, &context(), &target(), identity, "data")
            .is_ok()
    );
}

#[tokio::test]
async fn whole_file_hash_work_is_reserved_for_tiny_pages_and_released_slots_on_cancel() {
    let root = tempfile::tempdir().unwrap();
    let console = console(root.path());
    let principal = context().principal;
    let read = request("data", None, 1);
    for _ in 0..4 {
        drop(quota::Guard::admit(Arc::clone(&console), &principal, &read).unwrap());
    }
    assert!(quota::Guard::admit(Arc::clone(&console), &principal, &read).is_err());
    let a = quota::Guard::admit(Arc::clone(&console), &HostPrincipal::Socket(2), &read).unwrap();
    let b = quota::Guard::admit(Arc::clone(&console), &HostPrincipal::Socket(2), &read).unwrap();
    assert!(quota::Guard::admit(Arc::clone(&console), &HostPrincipal::Socket(2), &read).is_err());
    drop(a);
    drop(b);
    assert!(quota::Guard::admit(Arc::clone(&console), &HostPrincipal::Socket(2), &read).is_ok());
}

#[tokio::test]
async fn listing_uses_ignore_rules_and_literal_search_is_inert_and_bounded() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".gitignore"), "ignored\n").unwrap();
    std::fs::write(root.path().join("ignored"), "needle").unwrap();
    std::fs::write(root.path().join("text"), "needle.*\nNEEDLE.*\n").unwrap();
    std::fs::write(root.path().join("binary"), [0, 255]).unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::write(root.path().join(".git/config"), "needle.*").unwrap();
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
    assert!(
        entries
            .iter()
            .all(|e| e.path != "ignored" && !e.path.starts_with(".git/"))
    );
    assert!(!summary.truncated);
    let HostResponse::FilesSearch { matches, summary } = console
        .inspect(
            &context(),
            HostRequest::FilesSearch {
                target: target(),
                path: ".".into(),
                literal: "needle.*".into(),
                match_case: Some(false),
            },
            workspace(root.path()),
        )
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert_eq!(matches.len(), 2);
    assert_eq!(summary.skipped_binary, 1);
    assert!(matches.iter().all(|m| m.path == "text"));
    assert!(
        validate(&HostRequest::FilesSearch {
            target: target(),
            path: ".".into(),
            literal: "x".repeat(1025),
            match_case: None
        })
        .is_err()
    );
}

#[tokio::test]
async fn expired_execution_budget_and_cancellation_stop_backend_work() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("data"), "content").unwrap();
    let workspace = workspace(root.path());
    let token = plakous::model::CancellationToken::new();
    let control = OperationContext::new(token.clone())
        .with_budget(plakous::model::Timestamp::new(1, 0).unwrap());
    assert_eq!(
        workspace
            .environment
            .read_workspace(
                WorkspaceReadSliceRequest::start("data", 1).unwrap(),
                control
            )
            .await
            .unwrap_err()
            .kind(),
        ExecutionBackendErrorKind::TimedOut
    );
    token.cancel();
    assert_eq!(
        workspace
            .environment
            .read_workspace(
                WorkspaceReadSliceRequest::start("data", 1).unwrap(),
                OperationContext::new(token)
            )
            .await
            .unwrap_err()
            .kind(),
        ExecutionBackendErrorKind::Cancelled
    );
}

#[tokio::test]
async fn listing_marks_the_entry_and_output_caps_instead_of_claiming_clean() {
    let root = tempfile::tempdir().unwrap();
    for n in 0..1_001 {
        std::fs::write(root.path().join(format!("file-{n:04}")), "").unwrap();
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
    assert_eq!(entries.len(), 1_000);
    assert!(summary.truncated);
    let HostResponse::FilesSearch { summary, .. } = console
        .inspect(
            &context(),
            HostRequest::FilesSearch {
                target: target(),
                path: ".".into(),
                literal: "absent".into(),
                match_case: None,
            },
            workspace(root.path()),
        )
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert_eq!(summary.stop_reason, Some(FileStopReason::Files));
    let mut summary = FileQuerySummary {
        skipped_entries: 0,
        skipped_binary: 0,
        skipped_oversize: 0,
        skipped_changed: 0,
        truncated: false,
        stop_reason: None,
    };
    let rows = bounded_rows(vec!["x".repeat(2048); 200], &mut summary).unwrap();
    assert!(rows.len() < 200 && summary.truncated);
    assert_eq!(summary.stop_reason, Some(FileStopReason::ResultBytes));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_reads_do_not_open_a_session_or_fallback_from_unavailable_placement() {
    use plakous_hosted::host_service::wire::HostSessions;
    let (_root, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let work = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(work.path()).unwrap();
    std::fs::write(canonical.join("data"), "selected-workspace").unwrap();
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
            Arc::new(plakous::runner::WorkspaceCatalog::local(None, &[], paths.home()).unwrap()),
            0,
        )
        .unwrap();
    let mut record = crate::store::sessions::SessionRecord::new(canonical.clone());
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
    let request = |node| HostRequest::FilesRead {
        target: WorkspaceTarget {
            session: record.id.clone(),
            node,
        },
        path: "data".into(),
        cursor: None,
        max_bytes: None,
    };
    let revision = record.store_revision;
    assert!(!paths.runtime_database().exists());
    let mut web = context();
    web.principal = HostPrincipal::Web("verified".into());
    assert!(matches!(
        sessions.host_request(web, request(None)).await,
        Err(RequestRefusal::CapabilityDisabled {
            capability: ConsoleCapability::FilesRead
        })
    ));
    assert!(matches!(
        sessions
            .host_request(context(), request(None))
            .await
            .unwrap(),
        HostResponse::FilesRead { .. }
    ));
    assert_eq!(
        sessions
            .host_request(context(), request(Some("missing-child".into())))
            .await
            .unwrap(),
        refused(ReadRefusal::WorkspaceUnavailable {})
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
    let mut record = crate::store::sessions::SessionRecord::new(canonical.clone());
    record.runner = Some(plakous_hosted::sessions::RunnerDisplay {
        name: "unavailable".into(),
        runner_id: "saved-unavailable".into(),
        workspace_root: canonical.clone(),
        workspace_cwd: canonical,
    });
    services
        .session_records()
        .unwrap()
        .save(&mut record)
        .unwrap();
    let request = HostRequest::FilesRead {
        target: WorkspaceTarget {
            session: record.id.clone(),
            node: None,
        },
        path: "data".into(),
        cursor: None,
        max_bytes: None,
    };
    assert!(matches!(
        sessions.host_request(context(), request).await,
        Err(RequestRefusal::PlacementRefused { .. })
    ));
    assert!(!paths.runtime_database().exists());
}

#[tokio::test]
async fn symlink_swap_never_discloses_the_outside_file() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "outside-bytes").unwrap();
    std::fs::write(root.path().join("data"), "inside-bytes").unwrap();
    let file = root.path().join("data");
    let swap = root.path().join("swap");
    let secret = outside.path().join("secret");
    let worker = std::thread::spawn(move || {
        for _ in 0..200 {
            std::os::unix::fs::symlink(&secret, &swap).unwrap();
            std::fs::rename(&swap, &file).unwrap();
            std::fs::write(&swap, "inside-bytes").unwrap();
            std::fs::rename(&swap, &file).unwrap();
        }
    });
    let console = console(root.path());
    for _ in 0..200 {
        if let HostResponse::FilesRead { content } = console
            .inspect(
                &context(),
                request("data", None, 100),
                workspace(root.path()),
            )
            .await
            .unwrap()
        {
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(content.bytes_b64)
                    .unwrap(),
                b"inside-bytes"
            );
        }
    }
    worker.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equal_pathnames_on_two_runners_never_read_the_local_file() {
    use plakous::tools::runner::{
        RunnerBackend, RunnerCall, RunnerCallFuture, RunnerClass, RunnerFailure, RunnerLink,
        RunnerVerb, runner_descriptor, runner_environment,
    };
    use plakous::tools::{
        ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId, WorkspaceContentIdentity,
    };
    struct ReadEndpoint(&'static [u8]);
    impl RunnerLink for ReadEndpoint {
        fn call(&self, call: RunnerCall) -> RunnerCallFuture {
            assert!(matches!(call.class(), RunnerClass::Unkeyed));
            let answer = if call.verb() == RunnerVerb::ReadWorkspace {
                assert_eq!(call.payload()["request"]["path"], "data");
                let content = WorkspaceContentIdentity::from_bytes(self.0).unwrap();
                Ok(
                    serde_json::json!({"content":{"byte_len":content.byte_len(),"digest":content.digest()},"offset":0,"bytes":plakous::tools::runner::hex(self.0),"next":null}),
                )
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
    let root = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    std::fs::write(canonical.join("data"), "must-not-read-local").unwrap();
    let console = console(root.path());
    for (id, bytes) in [
        ("first", b"first-runner".as_slice()),
        ("second", b"second-runner".as_slice()),
    ] {
        let direct = workspace(&canonical);
        let descriptor =
            runner_descriptor(&direct.environment, RunnerId::new(id).unwrap()).unwrap();
        let backend =
            Arc::new(RunnerBackend::new(descriptor, Arc::new(ReadEndpoint(bytes))).unwrap());
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
        let placed = resolve::Workspace {
            environment,
            identity: id.into(),
            placement: None,
            target: target(),
        };
        let HostResponse::FilesRead { content } = console
            .inspect(&context(), request("data", None, PAGE), placed)
            .await
            .unwrap()
        else {
            panic!("content")
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(content.bytes_b64)
                .unwrap(),
            bytes
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dormant_child_reads_neither_mint_missing_workspaces_nor_reuse_reclaimed_paths() {
    use plakous_host_protocol::topology::{WorkspaceStatus, WriterMode};
    use plakous_hosted::host_service::wire::HostSessions;
    use plakous_recursive_subagents::tree::{FakeVcs, ProductAgentTree, TreeCustody, TreeStore};
    let (_root, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let work = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(work.path()).unwrap();
    let child_path = canonical.join("child");
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
            Arc::new(plakous::runner::WorkspaceCatalog::local(None, &[], paths.home()).unwrap()),
            0,
        )
        .unwrap();
    let mut record = crate::store::sessions::SessionRecord::new(canonical.clone());
    record.node = plakous_hosted::sessions::SessionNode {
        tree_id: "tree".into(),
        agent_id: "root".into(),
    };
    services
        .session_records()
        .unwrap()
        .save(&mut record)
        .unwrap();
    let tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::new(FakeVcs::new()),
        "tree",
        "root",
        "owner",
        canonical,
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let store = crate::store::tree::open_tree_store(&paths).unwrap();
    let claim = store
        .take_custody(
            "tree",
            plakous_recursive_subagents::tree::custody_host(),
            None,
        )
        .unwrap();
    let sessions = crate::host::sessions::ProductSessions::new(
        &services,
        None,
        Some(tokio::runtime::Handle::current()),
    )
    .unwrap();
    let mut stored = 0;
    for (index, status) in [WorkspaceStatus::Active, WorkspaceStatus::CleanRemoved]
        .into_iter()
        .enumerate()
    {
        let mut snapshot =
            child_snapshot(&tree, &child_path, status, tree.revision + index as u64 + 1);
        snapshot.stored_revision = stored;
        stored = store.save(&snapshot, &claim).unwrap().value();
        if status == WorkspaceStatus::CleanRemoved {
            std::fs::create_dir(&child_path).unwrap();
            std::fs::write(child_path.join("data"), "unrelated-reuse").unwrap();
        }
        let request = HostRequest::FilesRead {
            target: WorkspaceTarget {
                session: record.id.clone(),
                node: Some("child".into()),
            },
            path: "data".into(),
            cursor: None,
            max_bytes: None,
        };
        assert_eq!(
            sessions.host_request(context(), request).await.unwrap(),
            refused(ReadRefusal::WorkspaceUnavailable {})
        );
        assert_eq!(child_path.exists(), status == WorkspaceStatus::CleanRemoved);
        assert!(!paths.runtime_database().exists());
    }
}

fn child_snapshot(
    tree: &plakous_recursive_subagents::tree::ProductAgentTree,
    path: &std::path::Path,
    status: plakous_host_protocol::topology::WorkspaceStatus,
    revision: u64,
) -> plakous_recursive_subagents::tree::ProductAgentTree {
    let mut nodes = tree.nodes().clone();
    let mut child = tree.node("root").unwrap().clone();
    child.agent_id = "child".into();
    child.parent_agent_id = Some("root".into());
    child.depth = 1;
    child.path = path.to_path_buf();
    child.workspace_id = "ws-child".into();
    nodes.insert("child".into(), child);
    let mut workspaces = tree.workspaces().clone();
    let mut workspace = workspaces["ws-root"].clone();
    workspace.workspace_id = "ws-child".into();
    workspace.owner_agent_id = "child".into();
    workspace.path = path.to_path_buf();
    workspace.status = status;
    workspaces.insert("ws-child".into(), workspace);
    plakous_recursive_subagents::tree::PersistedTree::from_rows(
        tree.tree_id.clone(),
        tree.root_agent_id.clone(),
        revision,
        tree.host_generation,
        tree.state,
        tree.writer_mode,
        tree.selected_tuple.clone(),
        tree.launch_checkout.clone(),
        tree.header_snapshot().clone(),
        nodes,
        tree.grants().clone(),
        tree.attempts().clone(),
        tree.candidates().clone(),
        workspaces,
        tree.messages().clone(),
        tree.operations().clone(),
    )
    .rows()
    .clone()
}

#[tokio::test]
async fn search_caps_distinguish_match_file_and_total_input_omissions() {
    let root = tempfile::tempdir().unwrap();
    let console = console(root.path());
    std::fs::write(root.path().join("text"), "needle\n".repeat(201)).unwrap();
    let search = || HostRequest::FilesSearch {
        target: target(),
        path: ".".into(),
        literal: "needle".into(),
        match_case: None,
    };
    let HostResponse::FilesSearch { matches, summary } = console
        .inspect(&context(), search(), workspace(root.path()))
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert_eq!(matches.len(), 200);
    assert_eq!(summary.stop_reason, Some(FileStopReason::Matches));
    std::fs::write(
        root.path().join("text"),
        format!("needle{}", "x".repeat(3_000)),
    )
    .unwrap();
    let HostResponse::FilesSearch { matches, .. } = console
        .inspect(&context(), search(), workspace(root.path()))
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert!(matches[0].truncated);
    assert!(
        base64::engine::general_purpose::STANDARD
            .decode(&matches[0].bytes_b64)
            .unwrap()
            .len()
            <= 2048
    );
    std::fs::write(root.path().join("text"), "x".repeat(1024 * 1024 + 1)).unwrap();
    let HostResponse::FilesSearch { summary, .. } = console
        .inspect(&context(), search(), workspace(root.path()))
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert_eq!(summary.skipped_oversize, 1);
    std::fs::remove_file(root.path().join("text")).unwrap();
    let input = "x".repeat(1024 * 1024);
    for n in 0..17 {
        std::fs::write(root.path().join(format!("file-{n:02}")), &input).unwrap();
    }
    let HostResponse::FilesSearch { summary, .. } = console
        .inspect(&context(), search(), workspace(root.path()))
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert_eq!(summary.stop_reason, Some(FileStopReason::ScannedBytes));
}

#[tokio::test]
async fn ignored_directory_entries_still_exhaust_the_traversal_budget() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".gitignore"), "*\n").unwrap();
    for n in 0..10 {
        std::fs::write(root.path().join(format!("ignored-{n}")), "needle").unwrap();
    }
    let limits =
        RepositoryQueryLimits::new(1000, 3, 1024 * 1024, INPUT, 200, 2048, PAGE as usize).unwrap();
    let query = RepositoryQuery::new(
        ".",
        b"needle".to_vec(),
        RepositoryMatchCase::Sensitive,
        limits,
    )
    .unwrap();
    let result = workspace(root.path())
        .environment
        .query_repository(
            query,
            OperationContext::new(plakous::model::CancellationToken::new()),
        )
        .await
        .unwrap();
    assert_eq!(result.matches(), []);
    assert_eq!(
        result.stop_reason(),
        Some(RepositoryQueryStopReason::TreeEntries)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_drops_pending_runner_io_and_cancels_the_read_control() {
    use plakous::tools::runner::{
        RunnerBackend, RunnerCall, RunnerCallFuture, RunnerFailure, RunnerLink, RunnerVerb,
        runner_descriptor, runner_environment,
    };
    use plakous::tools::{ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct PendingRead {
        entered: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicBool>,
        cancellation: Arc<std::sync::Mutex<Option<plakous::model::CancellationToken>>>,
    }
    struct DropRead(Arc<AtomicBool>);
    impl Drop for DropRead {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    impl RunnerLink for PendingRead {
        fn call(&self, call: RunnerCall) -> RunnerCallFuture {
            if call.verb() == RunnerVerb::ReadWorkspace {
                *self.cancellation.lock().unwrap() = Some(call.cancellation().clone());
                let entered = Arc::clone(&self.entered);
                let dropped = Arc::clone(&self.dropped);
                return Box::pin(async move {
                    let _drop = DropRead(dropped);
                    entered.notify_one();
                    std::future::pending().await
                });
            }
            let answer = match call.payload()["op"].as_str() {
                Some("canonicalize") => Ok(serde_json::json!({"path":call.payload()["path"]})),
                Some("filesystem_identity") => Ok(serde_json::json!({"device":1,"inode":2})),
                _ => Err(RunnerFailure::Unsupported),
            };
            Box::pin(std::future::ready(answer))
        }
    }
    let root = tempfile::tempdir().unwrap();
    let direct = workspace(root.path());
    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let cancellation = Arc::new(std::sync::Mutex::new(None));
    let link = PendingRead {
        entered: Arc::clone(&entered),
        dropped: Arc::clone(&dropped),
        cancellation: Arc::clone(&cancellation),
    };
    let descriptor =
        runner_descriptor(&direct.environment, RunnerId::new("pending").unwrap()).unwrap();
    let backend = Arc::new(RunnerBackend::new(descriptor, Arc::new(link)).unwrap());
    let scope = ExecutionEnvironmentScope::Run(
        plakous::model::RunId::try_from(uuid::Uuid::new_v4()).unwrap(),
    );
    let environment = runner_environment(
        scope,
        root.path(),
        root.path(),
        ExecutionEnvironmentConfig::default(),
        backend,
    )
    .unwrap();
    let placed = resolve::Workspace {
        environment,
        identity: "pending".into(),
        placement: None,
        target: target(),
    };
    let console = console(root.path());
    let task = tokio::spawn(async move {
        console
            .inspect(&context(), request("data", None, PAGE), placed)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(dropped.load(Ordering::Acquire));
    assert!(
        cancellation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
}
