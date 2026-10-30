use super::*;
use serde_json::json;

fn environment(root: &std::path::Path) -> TestResult<Arc<ExecutionEnvironment>> {
    Ok(Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids(1)?.run),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?))
}

fn unread(path: &str) -> String {
    plakous_tool_files::PLAKOUS_TOOL_WRITE_UNREAD.replace("{path}", &json!(path).to_string())
}

fn stale(path: &str) -> String {
    plakous_tool_files::PLAKOUS_TOOL_WRITE_STALE.replace("{path}", &json!(path).to_string())
}

#[tokio::test]
async fn oversized_codex_named_listing_refuses_before_any_file_is_created() -> TestResult {
    let root = tempfile::tempdir()?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let component = "\"".repeat(200);
    let mut patch = String::from("*** Begin Patch\n");
    for index in 0..256 {
        writeln!(
            patch,
            "*** Add File: {index:03}-{component}/{component}\n+created"
        )?;
    }
    patch.push_str("*** End Patch\n");
    let result = execute(
        &registry,
        environment(root.path())?,
        ids(1)?,
        "apply_patch",
        &json!({"patch":patch}).to_string(),
    )
    .await;
    assert_eq!(fs::read_dir(root.path())?.count(), 0);
    let result = result?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        text(&result)?,
        "file change result exceeds the text bound; split the transaction"
    );
    Ok(())
}

#[tokio::test]
async fn unreadable_write_targets_refuse_with_actionable_remedies() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    builder.register(plakous_tool_files::FilesToolPack::edit_files_registration()?)?;
    let registry = builder.freeze();
    let environment = environment(root.path())?;
    fs::write(root.path().join("binary"), b"a\0b")?;
    let large = fs::File::create(root.path().join("large"))?;
    large.set_len(16 * 1024 * 1024 + 1)?;
    fs::create_dir(root.path().join("directory"))?;
    for (index, path, message) in [
        (
            1,
            "binary",
            "Write refused: \"binary\" is binary and the file-reading tool cannot read it. Delete it in a separate tool call, then create it again; no files were changed.",
        ),
        (
            2,
            "large",
            "File change refused: \"large\" exceeds the readable file-size limit. Use a process tool to remove or shrink it, then retry; no files were changed.",
        ),
        (
            3,
            "directory",
            "File change refused: \"directory\" is a directory. Choose a file path, then retry; no files were changed.",
        ),
    ] {
        let result = execute(&registry, environment.clone(), ids(index)?, "apply_patch", &json!({"file_path":path,"old_string":"","new_string":"replacement","replace_all":true}).to_string()).await?;
        assert_eq!(result.kind(), ToolOutcomeKind::Failure);
        assert_eq!(text(&result)?, message);
        let result = execute(
            &registry,
            environment.clone(),
            ids(index + 10)?,
            "edit_files",
            &json!({"ops":[{"op":"write","path":path,"content":"replacement"}]}).to_string(),
        )
        .await?;
        assert_eq!(result.kind(), ToolOutcomeKind::Failure);
        let expected = if path == "binary" {
            message.to_owned()
        } else {
            format!("op 1: {message}")
        };
        assert_eq!(text(&result)?, expected);
    }
    assert_eq!(fs::read(root.path().join("binary"))?, b"a\0b");
    assert_eq!(large.metadata()?.len(), 16 * 1024 * 1024 + 1);
    assert!(root.path().join("directory").is_dir());
    // The binary refusal's remedy is usable without authorizing a blind write.
    let result = execute(
        &registry,
        environment.clone(),
        ids(4)?,
        "apply_patch",
        &json!({"patch":"*** Begin Patch\n*** Delete File: binary\n*** End Patch\n"}).to_string(),
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Success);
    let result = execute(
        &registry,
        environment,
        ids(5)?,
        "apply_patch",
        r#"{"file_path":"binary","old_string":"","new_string":"text","replace_all":true}"#,
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Success);
    assert_eq!(fs::read(root.path().join("binary"))?, b"text");
    Ok(())
}

#[tokio::test]
async fn blind_write_requires_fresh_read_and_consumes_it_even_for_noop() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "old\nsecond\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = environment(root.path())?;
    let write = json!({"file_path":"a", "old_string":"", "new_string":"new", "replace_all":true})
        .to_string();
    let result = execute(
        &registry,
        environment.clone(),
        ids(1)?,
        "apply_patch",
        &write,
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    assert_eq!(text(&result)?, unread("a"));
    assert_eq!(fs::read(root.path().join("a"))?, b"old\nsecond\n");
    let read = execute(
        &registry,
        environment.clone(),
        ids(2)?,
        "read_file",
        r#"{"path":"a","limit":1}"#,
    )
    .await?;
    assert_eq!(read.kind(), ToolOutcomeKind::Success);
    let mut next_turn = ids(3)?;
    next_turn.turn = TurnId::from_str("00000000-0000-0000-0000-000000000099")?;
    let result = execute(
        &registry,
        Arc::new((*environment).clone()),
        next_turn,
        "apply_patch",
        &write,
    )
    .await?;
    assert_eq!(text(&result)?, "changed_files=1\nM \"a\"");
    assert_eq!(fs::read(root.path().join("a"))?, b"new");
    let result = execute(
        &registry,
        environment.clone(),
        ids(4)?,
        "apply_patch",
        &write,
    )
    .await?;
    assert_eq!(text(&result)?, unread("a"));
    execute(
        &registry,
        environment.clone(),
        ids(5)?,
        "read_file",
        r#"{"path":"a"}"#,
    )
    .await?;
    let result = execute(
        &registry,
        environment.clone(),
        ids(6)?,
        "apply_patch",
        &write,
    )
    .await?;
    assert_eq!(text(&result)?, "changed_files=0");
    let result = execute(&registry, environment, ids(7)?, "apply_patch", &write).await?;
    assert_eq!(text(&result)?, unread("a"));
    Ok(())
}

#[tokio::test]
async fn same_size_external_edit_is_stale_but_identical_content_is_fresh() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "old")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = environment(root.path())?;
    execute(
        &registry,
        environment.clone(),
        ids(1)?,
        "read_file",
        r#"{"path":"a"}"#,
    )
    .await?;
    fs::write(root.path().join("a"), "new")?;
    let write = r#"{"file_path":"a","old_string":"","new_string":"done","replace_all":true}"#;
    let result = execute(
        &registry,
        environment.clone(),
        ids(2)?,
        "apply_patch",
        write,
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    assert_eq!(text(&result)?, stale("a"));
    assert_eq!(fs::read(root.path().join("a"))?, b"new");
    execute(
        &registry,
        environment.clone(),
        ids(3)?,
        "read_file",
        r#"{"path":"a"}"#,
    )
    .await?;
    // New inode and mtime, identical content: identity is content-based.
    fs::write(root.path().join("replacement"), "new")?;
    fs::rename(root.path().join("replacement"), root.path().join("a"))?;
    let result = execute(&registry, environment, ids(4)?, "apply_patch", write).await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Success);
    Ok(())
}

#[tokio::test]
async fn implicit_patch_reads_failed_reads_and_new_attachments_do_not_authorize_write() -> TestResult
{
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "old")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let attached = environment(root.path())?;
    let patch = r#"{"file_path":"a","old_string":"old","new_string":"old"}"#;
    execute(&registry, attached.clone(), ids(1)?, "apply_patch", patch).await?;
    let write = r#"{"file_path":"a","old_string":"","new_string":"new","replace_all":true}"#;
    let result = execute(&registry, attached.clone(), ids(2)?, "apply_patch", write).await?;
    assert_eq!(text(&result)?, unread("a"));
    let result = execute(
        &registry,
        attached.clone(),
        ids(3)?,
        "read_file",
        r#"{"path":"a","offset":99}"#,
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    let result = execute(&registry, attached.clone(), ids(4)?, "apply_patch", write).await?;
    assert_eq!(text(&result)?, unread("a"));
    execute(
        &registry,
        attached.clone(),
        ids(5)?,
        "read_file",
        r#"{"path":"a"}"#,
    )
    .await?;
    let result = execute(
        &registry,
        environment(root.path())?,
        ids(6)?,
        "apply_patch",
        write,
    )
    .await?;
    assert_eq!(text(&result)?, unread("a"));
    fs::write(root.path().join("binary"), b"a\0b")?;
    let result = execute(
        &registry,
        attached.clone(),
        ids(7)?,
        "read_file",
        r#"{"path":"binary"}"#,
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    let result = execute(
        &registry,
        attached,
        ids(8)?,
        "apply_patch",
        r#"{"file_path":"binary","old_string":"","new_string":"new","replace_all":true}"#,
    )
    .await?;
    assert_eq!(
        text(&result)?,
        "Write refused: \"binary\" is binary and the file-reading tool cannot read it. Delete it in a separate tool call, then create it again; no files were changed."
    );
    Ok(())
}

#[tokio::test]
async fn named_changes_use_committed_dispositions_and_escape_paths() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("source"), "old\n")?;
    fs::write(root.path().join("unchanged"), "same\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = environment(root.path())?;
    let patch = "*** Begin Patch\n*** Update File: source\n*** Move to: destination\n@@\n-old\n+new\n*** Update File: unchanged\n@@\n-same\n+same\n*** Add File: a/file\n+nested\n*** Add File: a-file\n+flat\n*** End Patch\n";
    let result = execute(
        &registry,
        environment.clone(),
        ids(1)?,
        "apply_patch",
        &json!({"patch":patch}).to_string(),
    )
    .await?;
    assert_eq!(
        text(&result)?,
        "changed_files=4\nA \"a-file\"\nA \"a/file\"\nA \"destination\"\nD \"source\""
    );
    let escaped_name = "space \"quote\"\nnext";
    let result = execute(
        &registry,
        environment,
        ids(2)?,
        "apply_patch",
        &json!({"file_path":escaped_name,"old_string":"","new_string":"created","replace_all":true})
            .to_string(),
    )
    .await?;
    assert_eq!(
        text(&result)?,
        format!("changed_files=1\nA {}", json!(escaped_name))
    );
    assert_eq!(text(&result)?.lines().count(), 2);
    Ok(())
}

#[tokio::test]
async fn sdk_write_expect_cannot_bypass_read_guard_and_batch_is_zero_effect() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "old")?;
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    builder.register(plakous_tool_files::FilesToolPack::edit_files_registration()?)?;
    let registry = builder.freeze();
    let environment = environment(root.path())?;
    let revision = plakous_tools::WorkspaceContentIdentity::from_bytes(b"old")?
        .digest()
        .to_string();
    let args = json!({"ops":[{"op":"write","path":"new","content":"created"}, {"op":"write","path":"a","content":"next","expect":revision}]}).to_string();
    let result = execute(&registry, environment.clone(), ids(1)?, "edit_files", &args).await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    assert_eq!(text(&result)?, unread("a"));
    assert!(!root.path().join("new").exists());
    assert_eq!(fs::read(root.path().join("a"))?, b"old");
    execute(
        &registry,
        environment.clone(),
        ids(2)?,
        "read_file",
        r#"{"path":"a"}"#,
    )
    .await?;
    let result = execute(&registry, environment.clone(), ids(3)?, "edit_files", &args).await?;
    assert_eq!(text(&result)?, "changed_files=2\nM \"a\"\nA \"new\"");
    let result = execute(&registry, environment, ids(4)?, "edit_files", r#"{"ops":[{"op":"write","path":"created","content":"one"},{"op":"write","path":"created","content":"two"}]}"#).await?;
    assert_eq!(text(&result)?, "changed_files=1\nA \"created\"");
    assert_eq!(fs::read(root.path().join("created"))?, b"two");
    Ok(())
}

#[tokio::test]
async fn direct_and_nested_write_vocabularies_share_guard_and_named_typed_results() -> TestResult {
    use plakous_compose::CodingPresentationExt as _;
    use plakous_tools::{CodingPresentation, JobConfig, ProviderModelProjectionIdentityV2};
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    let registry = builder.freeze();
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"write-guard-vocabularies"],
    )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        plakous_model::IntegrationClass::Standard,
        "test",
        "test",
        "test",
        "test",
        "test",
        "test",
        effective,
        1,
    )?;
    for (presentation, read, write, read_args, write_args) in [
        (
            CodingPresentation::Claude,
            "Read",
            "Write",
            json!({"file_path":"a","limit":1}),
            json!({"file_path":"a","content":"new"}),
        ),
        (
            CodingPresentation::Grok,
            "read_file",
            "write",
            json!({"target_file":"a","limit":1}),
            json!({"file_path":"a","content":"new"}),
        ),
        (
            CodingPresentation::Pi,
            "read",
            "write",
            json!({"path":"a","limit":1}),
            json!({"path":"a","content":"new"}),
        ),
    ] {
        for nested in [false, true] {
            let root = tempfile::tempdir()?;
            fs::write(root.path().join("a"), "old\nsecond\n")?;
            let environment = environment(root.path())?;
            let projection = if nested {
                presentation.compile_nested_projection(
                    JobConfig::default(),
                    registry.clone(),
                    target.clone(),
                    &[ToolName::new(read)?, ToolName::new(write)?],
                )?
            } else {
                presentation.projection_map(
                    JobConfig::default(),
                    registry.clone(),
                    target.clone(),
                )?
            };
            for case in [
                (1, write, write_args.clone(), Some(unread("a"))),
                (2, read, read_args.clone(), None),
                (
                    3,
                    write,
                    write_args.clone(),
                    Some("changed_files=1\nM \"a\"".into()),
                ),
            ] {
                check_projected_call(&registry, environment.clone(), &projection, case).await?;
            }
            assert_eq!(fs::read(root.path().join("a"))?, b"new");
        }
    }
    Ok(())
}

async fn check_projected_call(
    registry: &plakous_tools::FrozenToolRegistry,
    environment: Arc<ExecutionEnvironment>,
    projection: &plakous_tools::ToolProjectionMap,
    (call, name, args, expected): (u8, &str, serde_json::Value, Option<String>),
) -> TestResult {
    let ids = ids(call)?;
    let (invocation, receipt) = projection
        .prepare(ToolInvocationRequest::new(
            ids.run,
            ids.turn,
            ids.attempt,
            ids.call,
            ToolName::new(name)?,
            ToolCorrelation::default(),
            args.to_string(),
        ))?
        .into_parts();
    let mut outcome = execute_invocation(
        registry,
        environment.clone(),
        ids,
        invocation.with_want_value(true),
        CancellationToken::new(),
    )
    .await?;
    if call == 3 {
        let Some(plakous_tools::ExecutionValue::FileChanges(value)) = outcome.take_value() else {
            return Err("missing typed changes".into());
        };
        assert_eq!(value.changes.len(), 1);
        assert_eq!(value.changes[0].path, "a");
        assert_eq!(
            value.changes[0].revision,
            Some(plakous_tools::WorkspaceContentIdentity::from_bytes(b"new")?)
        );
    }
    let raised = projection.raise(&receipt.ok_or("missing receipt")?, outcome)?;
    if let Some(expected) = expected {
        if raised.kind() == ToolOutcomeKind::Failure || text(&raised)? == expected {
            assert_eq!(text(&raised)?, expected);
        } else {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(text(&raised)?)?["result"],
                expected
            );
        }
    }
    if call == 1 {
        assert_eq!(raised.kind(), ToolOutcomeKind::Failure);
        assert_eq!(fs::read(environment.root().join("a"))?, b"old\nsecond\n");
    }
    Ok(())
}

#[tokio::test]
async fn shell_and_search_reads_do_not_authorize_blind_writes() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "old")?;
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    builder.register(
        Arc::new(plakous_tool_process::ProcessSessionTools::posix_sh()?).exec_registration()?,
    )?;
    let registry = builder.freeze();
    let environment = environment(root.path())?;
    let result = execute(&registry, environment.clone(), ids(1)?, "process_session_exec", r#"{"argv":["/bin/sh","-c","cat a"],"workdir":".","yield_time_ms":1000,"max_output_bytes":1024}"#).await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Success);
    execute(
        &registry,
        environment.clone(),
        ids(2)?,
        "search_repository",
        r#"{"query":"old","path":"."}"#,
    )
    .await?;
    let result = execute(
        &registry,
        environment,
        ids(3)?,
        "apply_patch",
        r#"{"file_path":"a","old_string":"","new_string":"new","replace_all":true}"#,
    )
    .await?;
    assert_eq!(text(&result)?, unread("a"));
    assert_eq!(fs::read(root.path().join("a"))?, b"old");
    Ok(())
}

#[tokio::test]
async fn aliases_share_reads_but_moves_and_context_checked_edits_invalidate_them() -> TestResult {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "old\n")?;
    let registry = registry(Arc::new(InMemoryPlanState::new()))?;
    let environment = environment(root.path())?;
    let absolute = root.path().canonicalize()?.join("a");
    execute(
        &registry,
        environment.clone(),
        ids(1)?,
        "read_file",
        &json!({"path":absolute}).to_string(),
    )
    .await?;
    let result = execute(
        &registry,
        environment.clone(),
        ids(2)?,
        "apply_patch",
        r#"{"file_path":"./a","old_string":"","new_string":"old\n","replace_all":true}"#,
    )
    .await?;
    assert_eq!(text(&result)?, "changed_files=0");
    execute(
        &registry,
        environment.clone(),
        ids(3)?,
        "read_file",
        r#"{"path":"a"}"#,
    )
    .await?;
    let patch = "*** Begin Patch\n*** Update File: a\n*** Move to: b\n*** End Patch\n";
    execute(
        &registry,
        environment.clone(),
        ids(4)?,
        "apply_patch",
        &json!({"patch":patch}).to_string(),
    )
    .await?;
    fs::write(root.path().join("a"), "old\n")?;
    for path in ["a", "b"] {
        let result = execute(
            &registry,
            environment.clone(),
            ids(5)?,
            "apply_patch",
            &json!({"file_path":path,"old_string":"","new_string":"new","replace_all":true})
                .to_string(),
        )
        .await?;
        assert_eq!(text(&result)?, unread(path));
    }
    Ok(())
}

#[tokio::test]
async fn oversized_sdk_path_listing_refuses_before_any_target_is_created() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut builder = ToolRegistryBuilder::default();
    builder.register(plakous_tool_files::FilesToolPack::edit_files_registration()?)?;
    let registry = builder.freeze();
    let ops = (0..256).map(|index| json!({"op":"write","path":format!("{index:03}-{}", "\n".repeat(200)),"content":""})).collect::<Vec<_>>();
    let result = execute(
        &registry,
        environment(root.path())?,
        ids(1)?,
        "edit_files",
        &json!({"ops":ops}).to_string(),
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    assert_eq!(
        text(&result)?,
        "file change result exceeds the text bound; split the transaction"
    );
    assert_eq!(fs::read_dir(root.path())?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn writes_beneath_a_file_component_refuse_with_their_index_before_any_effect() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    builder.register(plakous_tool_files::FilesToolPack::edit_files_registration()?)?;
    let registry = builder.freeze();
    let environment = environment(root.path())?;
    fs::write(root.path().join("data.txt"), b"data")?;
    let message = "File change refused: \"data.txt/child\" has a parent component that is not a directory. Choose a path inside an existing directory, then retry; no files were changed.";
    let result = execute(
        &registry,
        environment.clone(),
        ids(1)?,
        "edit_files",
        &json!({"ops":[
            {"op":"write","path":"safe","content":"never published"},
            {"op":"write","path":"data.txt/child","content":"refused"}
        ]})
        .to_string(),
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    assert_eq!(text(&result)?, format!("op 2: {message}"));
    let result = execute(
        &registry,
        environment,
        ids(2)?,
        "apply_patch",
        &json!({"file_path":"data.txt/child","old_string":"","new_string":"refused","replace_all":true})
            .to_string(),
    )
    .await?;
    assert_eq!(result.kind(), ToolOutcomeKind::Failure);
    assert_eq!(text(&result)?, message);
    assert_eq!(fs::read(root.path().join("data.txt"))?, b"data");
    assert_eq!(fs::read_dir(root.path())?.count(), 1);
    Ok(())
}
