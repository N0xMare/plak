use super::{
    Arc, ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, TestResult,
    ToolOutcomeKind, execute, fs, ids,
};
use plakous_tool_files::FilesToolPack;
use plakous_tool_process::ProcessSessionTools;
use plakous_tool_search::SearchToolPack;
use plakous_tools::{FrozenToolRegistry, ToolRegistryBuilder, WorkspaceContentIdentity};
use serde_json::{Value, json};

fn sdk_registry() -> TestResult<FrozenToolRegistry> {
    let mut builder = ToolRegistryBuilder::default();
    for registration in <FilesToolPack as plakous_tools::ToolPack>::registrations(&FilesToolPack)? {
        builder.register(registration)?;
    }
    builder.register(FilesToolPack::edit_files_registration()?)?;
    builder.register(SearchToolPack::list_entries_registration()?)?;
    let process = Arc::new(ProcessSessionTools::posix_sh()?);
    for registration in process.clone().registrations()? {
        builder.register(registration)?;
    }
    builder.register(process.exec_registration()?)?;
    Ok(builder.freeze())
}

fn environment(root: &std::path::Path) -> TestResult<Arc<ExecutionEnvironment>> {
    Ok(Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(1)?.run),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?))
}

#[test]
fn global_ignore_configuration_cannot_change_repository_listing() -> TestResult {
    let home = tempfile::tempdir()?;
    let excludes = home.path().join("global-ignore");
    fs::write(&excludes, "visible\n*.rs\n")?;
    let config = home.path().join(".gitconfig");
    fs::write(
        &config,
        format!("[core]\nexcludesFile = {}\n", excludes.display()),
    )?;
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "coding_tools::sdk_tools::repository_listing_honors_workspace_ignores_depth_glob_and_symlinks", "--nocapture"])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("GIT_CONFIG_GLOBAL", config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    Ok(())
}

#[tokio::test]
async fn existing_paths_only_glob_keeps_offset_paging_past_nine_hundred() -> TestResult {
    let root = tempfile::tempdir()?;
    for index in 0..1100 {
        fs::write(root.path().join(format!("file{index:04}.txt")), "")?;
    }
    let registry = super::registry(Arc::new(plakous_tool_plan::InMemoryPlanState::new()))?;
    let result = execute(&registry, environment(root.path())?, ids(20)?, "search_repository", &json!({
        "path":".", "query":"", "filename_glob":"*.txt", "paths_only":true, "offset":900, "head_limit":20
    }).to_string()).await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Success);
    let output = result.joined_text();
    assert!(output.contains("file0900.txt"), "{output}");
    assert!(output.contains("file0919.txt"), "{output}");
    assert!(!output.contains("file0920.txt"));
    assert!(!output.contains("retained-result bound"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_edits_hardlinked_entries_independently() -> TestResult {
    use std::os::unix::fs::MetadataExt;
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "old\n")?;
    fs::hard_link(root.path().join("a"), root.path().join("b"))?;
    let registry = super::registry(Arc::new(plakous_tool_plan::InMemoryPlanState::new()))?;
    let result = execute(&registry, environment(root.path())?, ids(24)?, "apply_patch", &json!({
        "patch":"*** Begin Patch\n*** Update File: a\n@@\n-old\n+first\n*** Update File: b\n@@\n-old\n+second\n*** End Patch\n"
    }).to_string()).await?;
    assert_eq!(
        result.kind(),
        ToolOutcomeKind::Success,
        "{}",
        result.joined_text()
    );
    assert_eq!(fs::read_to_string(root.path().join("a"))?, "first\n");
    assert_eq!(fs::read_to_string(root.path().join("b"))?, "second\n");
    let first = fs::metadata(root.path().join("a"))?;
    let second = fs::metadata(root.path().join("b"))?;
    assert_ne!(first.ino(), second.ino());
    assert_eq!((first.nlink(), second.nlink()), (1, 1));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn edit_files_move_accepts_binary_and_documents_create_mode() -> TestResult {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    fs::write(&source, b"\xff\0\x80")?;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;
    let inode = fs::metadata(&source)?.ino();
    let result = execute(
        &sdk_registry()?,
        environment(root.path())?,
        ids(21)?,
        "edit_files",
        &json!({"ops":[
            {"op":"move", "path":"source", "to":"destination"}
        ]})
        .to_string(),
    )
    .await?;
    assert_eq!(
        result.kind(),
        ToolOutcomeKind::Success,
        "{}",
        result.joined_text()
    );
    assert_eq!(fs::read(root.path().join("destination"))?, b"\xff\0\x80");
    let metadata = fs::metadata(root.path().join("destination"))?;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_ne!(metadata.ino(), inode);
    assert!(!source.exists());
    Ok(())
}

#[tokio::test]
async fn edit_files_bounds_replacement_expansion_before_allocating() -> TestResult {
    let root = tempfile::tempdir()?;
    let original = "a".repeat(10_000);
    fs::write(root.path().join("a"), &original)?;
    let registry = sdk_registry()?;
    let result = execute(
        &registry,
        environment(root.path())?,
        ids(18)?,
        "edit_files",
        &json!({"ops":[
            {"op":"replace","path":"a","old":"a","new":"x".repeat(1000),"all":true}
        ]})
        .to_string(),
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    assert!(result.joined_text().contains("op 1:"));
    assert_eq!(fs::read_to_string(root.path().join("a"))?, original);
    Ok(())
}

#[tokio::test]
async fn edit_files_validates_every_op_before_writing_and_names_the_failure() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("existing"), "original")?;
    let registry = sdk_registry()?;
    let environment = environment(root.path())?;
    let result = execute(
        &registry,
        environment,
        ids(1)?,
        "edit_files",
        &json!({"ops":[
            {"op":"write","path":"nested/new","content":"new"},
            {"op":"replace","path":"existing","old":"not present","new":"changed"}
        ]})
        .to_string(),
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    assert!(result.joined_text().contains("op 2:"));
    assert!(!root.path().join("nested").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("existing"))?,
        "original"
    );
    Ok(())
}

#[tokio::test]
async fn edit_files_orders_operations_and_uses_the_main_patch_move_parser() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("delete"), "remove")?;
    let registry = sdk_registry()?;
    let result = execute(&registry, environment(root.path())?, ids(2)?, "edit_files", &json!({"ops":[
        {"op":"write","path":"a","content":"old\n","expect":"absent"},
        {"op":"replace","path":"a","old":"old","new":"middle"},
        {"op":"move","path":"a","to":"b"},
        {"op":"patch","patch":"*** Begin Patch\n*** Update File: b\n*** Move to: nested/c\n@@\n-middle\n+final\n*** End Patch\n"},
        {"op":"delete","path":"delete"}
    ]}).to_string()).await?;
    assert_eq!(
        result.kind(),
        ToolOutcomeKind::Success,
        "{}",
        result.joined_text()
    );
    assert_eq!(fs::read_to_string(root.path().join("nested/c"))?, "final\n");
    for path in ["a", "b", "delete"] {
        assert!(!root.path().join(path).exists());
    }
    Ok(())
}

#[tokio::test]
async fn edit_files_revisions_and_move_destinations_are_preconditions() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "now")?;
    fs::write(root.path().join("b"), "occupied")?;
    let registry = sdk_registry()?;
    let environment = environment(root.path())?;
    let stale = WorkspaceContentIdentity::from_bytes(b"before")?
        .digest()
        .to_string();
    for (call, op) in [
        (
            3,
            json!({"op":"write","path":"a","content":"wrong","expect":stale}),
        ),
        (4, json!({"op":"move","path":"a","to":"b"})),
    ] {
        let result = execute(
            &registry,
            environment.clone(),
            ids(call)?,
            "edit_files",
            &json!({"ops":[op]}).to_string(),
        )
        .await?;
        assert_eq!(result.kind(), ToolOutcomeKind::Failure);
        assert_eq!(fs::read_to_string(root.path().join("a"))?, "now");
        assert_eq!(fs::read_to_string(root.path().join("b"))?, "occupied");
    }
    let revision = WorkspaceContentIdentity::from_bytes(b"now")?
        .digest()
        .to_string();
    execute(
        &registry,
        environment.clone(),
        ids(6)?,
        "read_file",
        r#"{"path":"a"}"#,
    )
    .await?;
    let result = execute(
        &registry,
        environment,
        ids(5)?,
        "edit_files",
        &json!({"ops":[
            {"op":"write","path":"a","content":"accepted","expect":revision}
        ]})
        .to_string(),
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Success);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn edit_files_refuses_escapes_symlinks_and_protected_paths_atomically() -> TestResult {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::os::unix::fs::symlink(outside.path(), root.path().join("link"))?;
    let registry = sdk_registry()?;
    let environment = environment(root.path())?;
    for (call, path) in [
        (6, "../escape"),
        (7, "link/escape"),
        (8, ".git/config"),
        (22, ".GIT/config"),
        (23, ".Jj/config"),
    ] {
        let result = execute(
            &registry,
            environment.clone(),
            ids(call)?,
            "edit_files",
            &json!({"ops":[
                {"op":"write","path":"safe","content":"must roll back"},
                {"op":"write","path":path,"content":"forbidden"}
            ]})
            .to_string(),
        )
        .await?;
        assert_eq!(
            result.kind(),
            ToolOutcomeKind::Failure,
            "{}",
            result.joined_text()
        );
        assert!(
            result.joined_text().starts_with("op 2: "),
            "{}",
            result.joined_text()
        );
        assert!(!root.path().join("safe").exists());
        assert!(!outside.path().join("escape").exists());
    }
    Ok(())
}

#[tokio::test]
async fn process_exec_preserves_literal_argv_and_defaults_stdin_to_eof() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = sdk_registry()?;
    let environment = environment(root.path())?;
    let literal = "$(touch injected); * ' \" $HOME";
    let result = execute(&registry, environment.clone(), ids(9)?, "process_session_exec", &json!({
        "argv":["printf","%s",literal],"workdir":".","yield_time_ms":1000,"max_output_bytes":40000
    }).to_string()).await?;
    let value: Value = serde_json::from_str(&result.joined_text())?;
    assert_eq!(value["output"], literal);
    assert_eq!(value["exit_code"], 0);
    assert!(!root.path().join("injected").exists());
    let result = execute(
        &registry,
        environment,
        ids(10)?,
        "process_session_exec",
        &json!({
            "argv":["/bin/cat"],"workdir":".","yield_time_ms":1000,"max_output_bytes":40000
        })
        .to_string(),
    )
    .await?;
    let value: Value = serde_json::from_str(&result.joined_text())?;
    assert_eq!(value["exit_code"], 0);
    assert!(value["session_id"].is_null());
    Ok(())
}

#[tokio::test]
async fn process_exec_keeps_non_tty_stdin_open_for_write_and_close() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = sdk_registry()?;
    let environment = environment(root.path())?;
    let result = execute(&registry, environment.clone(), ids(11)?, "process_session_exec", &json!({
        "argv":["/bin/cat"],"workdir":".","stdin":true,"yield_time_ms":0,"max_output_bytes":40000
    }).to_string()).await?;
    let value: Value = serde_json::from_str(&result.joined_text())?;
    let handle = value["session_id"]
        .as_u64()
        .ok_or("missing open-stdin session")?;
    let result = execute(
        &registry,
        environment,
        ids(12)?,
        "process_session_write",
        &json!({
            "session_id":handle,"input":"literal input\n","disposition":"close","yield_time_ms":1000
        })
        .to_string(),
    )
    .await?;
    let value: Value = serde_json::from_str(&result.joined_text())?;
    assert_eq!(value["output"], "literal input\n");
    assert_eq!(value["exit_code"], 0);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn repository_listing_honors_workspace_ignores_depth_glob_and_symlinks() -> TestResult {
    let parent = tempfile::tempdir()?;
    fs::write(parent.path().join(".ignore"), "visible\n")?;
    let root = parent.path().join("repo");
    fs::create_dir_all(root.join("src/deep"))?;
    fs::write(root.join(".gitignore"), "/ignored\n/src/hidden\n")?;
    fs::write(root.join("visible"), "")?;
    fs::write(root.join("ignored"), "")?;
    fs::write(root.join("src/hidden"), "")?;
    fs::write(root.join("src/main.rs"), "")?;
    fs::write(root.join("src/deep/lib.rs"), "")?;
    std::os::unix::fs::symlink(parent.path(), root.join("escape"))?;
    let registry = sdk_registry()?;
    let environment = environment(&root)?;
    for (call, args, expected) in [
        (
            13,
            json!({"path":".","depth":1}),
            vec![".gitignore", "src", "visible"],
        ),
        (
            14,
            json!({"path":"src","depth":1}),
            vec!["src/deep", "src/main.rs"],
        ),
        (
            15,
            json!({"path":"src","depth":2,"glob":"*.rs"}),
            vec!["src/deep/lib.rs", "src/main.rs"],
        ),
    ] {
        let result = execute(
            &registry,
            environment.clone(),
            ids(call)?,
            "list_entries",
            &args.to_string(),
        )
        .await?;
        let value: Value = serde_json::from_str(&result.joined_text())?;
        let entries = value["entries"].as_array().ok_or("missing entries")?;
        let paths: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry["path"].as_str())
            .collect();
        assert_eq!(paths, expected);
        assert_eq!(value["incomplete"], false);
        for entry in entries {
            assert_eq!(
                entry["kind"],
                if entry["path"] == "src" || entry["path"] == "src/deep" {
                    "directory"
                } else {
                    "file"
                }
            );
        }
    }
    assert!(
        execute(
            &registry,
            environment.clone(),
            ids(16)?,
            "list_entries",
            r#"{"path":"../"}"#
        )
        .await
        .is_err()
    );
    assert!(
        execute(
            &registry,
            environment,
            ids(17)?,
            "list_entries",
            r#"{"path":"escape"}"#
        )
        .await
        .is_err()
    );
    Ok(())
}
