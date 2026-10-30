use super::*;
use std::fs;

#[test]
fn discovers_global_then_root_to_cwd_with_exact_enrollment_inputs() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let root = temporary.path().join("repo");
    let nested = root.join("a/b");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&nested).unwrap();
    fs::write(home.join(INSTRUCTION_FILE_NAME), "global").unwrap();
    fs::write(root.join(INSTRUCTION_FILE_NAME), "root").unwrap();
    fs::write(root.join("a").join(INSTRUCTION_FILE_NAME), "a").unwrap();
    fs::write(nested.join(INSTRUCTION_FILE_NAME), "b").unwrap();

    let discovered = discover_instruction_sources(Some(&home), &root, &nested).unwrap();
    let sources = discovered.sources();
    assert_eq!(
        sources
            .iter()
            .map(DiscoveredInstructionSource::content)
            .collect::<Vec<_>>(),
        ["global", "root", "a", "b"]
    );
    assert_eq!(sources[0].source_id(), "plak.agents.global.v1");
    assert_eq!(
        instruction_source_scope_label(sources[0].source_id()),
        "global"
    );
    assert_eq!(
        instruction_source_scope_label(sources[1].source_id()),
        "workspace root"
    );
    assert_eq!(
        instruction_source_scope_label(sources[3].source_id()),
        "nested directory"
    );
    assert!(sources[3].source_id().starts_with("plak.agents.workspace-"));
    assert_eq!(
        sources[2].scope(),
        &InstructionScope::Workspace {
            relative_directory: PathBuf::from("a")
        }
    );
    assert!(sources[0].content_digest().starts_with("sha256:"));
    let registered = sources[3].registered_source().unwrap();
    assert_eq!(registered.source_id(), sources[3].source_id());
    assert_eq!(registered.content(), sources[3].content());
    assert_eq!(registered.version(), sources[3].content_digest());
    assert_eq!(
        registered.provenance(),
        format!("plak-agents-v1:{}", sources[3].scope().scope_label())
    );
}

#[test]
fn missing_optional_sources_are_not_errors() {
    let temporary = tempfile::tempdir().unwrap();
    let missing_home = temporary.path().join("missing-home");
    let discovered =
        discover_instruction_sources(Some(&missing_home), temporary.path(), temporary.path())
            .unwrap();
    assert_eq!(discovered.sources(), []);
}

#[test]
fn inlined_source_bytes_strip_the_composed_suffix_not_a_file_length() {
    let content = "repo agents";
    let composed = format!("core prompt\n\n{content}");
    assert_eq!(
        inlined_instruction_source_bytes(&composed, &[content]),
        "\n\nrepo agents".len() as u64
    );
    assert_eq!(
        inlined_instruction_source_bytes("core prompt only", &[content]),
        0,
        "facts that are not inlined must not split occupancy"
    );
    assert_eq!(
        inlined_instruction_source_bytes(content, &[content]),
        content.len() as u64
    );
}

#[cfg(unix)]
#[test]
fn rejects_instruction_symlinks_without_following_them() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let target = temporary.path().join("target");
    fs::write(&target, "outside").unwrap();
    symlink(&target, temporary.path().join(INSTRUCTION_FILE_NAME)).unwrap();

    let error = discover_instruction_sources(None, temporary.path(), temporary.path()).unwrap_err();
    assert!(matches!(
        error,
        InstructionDiscoveryError::UnsafePath { .. }
    ));
}

#[test]
fn rejects_cwd_outside_workspace_and_excessive_depth() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    assert!(matches!(
        discover_instruction_sources(None, &root, &outside),
        Err(InstructionDiscoveryError::InvalidWorkspaceBoundary { .. })
    ));

    let mut deep = root.clone();
    for index in 0..=MAX_WORKSPACE_DEPTH {
        deep.push(format!("d{index}"));
    }
    fs::create_dir_all(&deep).unwrap();
    assert_eq!(
        discover_instruction_sources(None, &root, &deep).unwrap_err(),
        InstructionDiscoveryError::WorkspaceDepthExceeded {
            max_edges: MAX_WORKSPACE_DEPTH
        }
    );
}

#[test]
fn enforces_aggregate_byte_bound_and_utf8() {
    let oversized = tempfile::tempdir().unwrap();
    fs::write(
        oversized.path().join(INSTRUCTION_FILE_NAME),
        vec![b'x'; MAX_DISCOVERED_CONTENT_BYTES + 1],
    )
    .unwrap();
    assert_eq!(
        discover_instruction_sources(None, oversized.path(), oversized.path()).unwrap_err(),
        InstructionDiscoveryError::ContentTooLarge {
            max_bytes: MAX_DISCOVERED_CONTENT_BYTES
        }
    );

    let invalid = tempfile::tempdir().unwrap();
    fs::write(invalid.path().join(INSTRUCTION_FILE_NAME), [0xff]).unwrap();
    assert!(matches!(
        discover_instruction_sources(None, invalid.path(), invalid.path()),
        Err(InstructionDiscoveryError::InvalidUtf8 { .. })
    ));
}

#[test]
fn content_digest_is_domain_separated_and_deterministic() {
    let first = source(
        "one".to_owned(),
        InstructionScope::Global,
        "same".to_owned(),
    );
    let second = source(
        "two".to_owned(),
        InstructionScope::Global,
        "same".to_owned(),
    );
    assert_eq!(first.content_digest(), second.content_digest());
    assert_eq!(first.content_digest().len(), 71);
}

#[test]
fn workspace_scope_ids_distinguish_same_depth_directories() {
    let left = InstructionScope::Workspace {
        relative_directory: PathBuf::from("left/child"),
    };
    let right = InstructionScope::Workspace {
        relative_directory: PathBuf::from("right/child"),
    };
    assert_ne!(left.scope_label(), right.scope_label());
    assert_ne!(left.provenance(), right.provenance());
}
