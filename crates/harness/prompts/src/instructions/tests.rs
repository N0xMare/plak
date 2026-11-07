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

    let discovered =
        discover_instruction_sources(&LocalInstructionSourceReader, Some(&home), &root, &nested)
            .unwrap();
    let sources = discovered.sources();
    assert_eq!(
        discovered,
        discover_instruction_sources_on(
            &LocalInstructionSourceReader,
            &LocalInstructionSourceReader,
            Some(&home),
            &root,
            &nested
        )
        .unwrap()
    );
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
fn equal_host_and_runner_paths_use_explicit_source_authorities() {
    use crate::test_readers::{RUNNER_ROOT, RemappedReader};
    let host = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    for (directory, content) in [(&host, "host-global"), (&remote, "runner-workspace")] {
        fs::create_dir(directory.path().join("repo")).unwrap();
        fs::write(
            directory.path().join("repo").join(INSTRUCTION_FILE_NAME),
            content,
        )
        .unwrap();
    }
    let path = Path::new(RUNNER_ROOT).join("repo");
    let discovered = discover_instruction_sources_on(
        &RemappedReader::new(remote.path()),
        &RemappedReader::new(host.path()),
        Some(&path),
        &path,
        &path,
    )
    .unwrap();
    assert_eq!(
        discovered
            .sources()
            .iter()
            .map(DiscoveredInstructionSource::content)
            .collect::<Vec<_>>(),
        ["host-global", "runner-workspace"]
    );
}

#[test]
fn missing_optional_sources_are_not_errors() {
    let temporary = tempfile::tempdir().unwrap();
    let missing_home = temporary.path().join("missing-home");
    let discovered = discover_instruction_sources(
        &LocalInstructionSourceReader,
        Some(&missing_home),
        temporary.path(),
        temporary.path(),
    )
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

    let error = discover_instruction_sources(
        &LocalInstructionSourceReader,
        None,
        temporary.path(),
        temporary.path(),
    )
    .unwrap_err();
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
        discover_instruction_sources(&LocalInstructionSourceReader, None, &root, &outside),
        Err(InstructionDiscoveryError::InvalidWorkspaceBoundary { .. })
    ));

    let mut deep = root.clone();
    for index in 0..=MAX_WORKSPACE_DEPTH {
        deep.push(format!("d{index}"));
    }
    fs::create_dir_all(&deep).unwrap();
    assert_eq!(
        discover_instruction_sources(&LocalInstructionSourceReader, None, &root, &deep)
            .unwrap_err(),
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
        discover_instruction_sources(
            &LocalInstructionSourceReader,
            None,
            oversized.path(),
            oversized.path()
        )
        .unwrap_err(),
        InstructionDiscoveryError::ContentTooLarge {
            max_bytes: MAX_DISCOVERED_CONTENT_BYTES
        }
    );

    let invalid = tempfile::tempdir().unwrap();
    fs::write(invalid.path().join(INSTRUCTION_FILE_NAME), [0xff]).unwrap();
    assert!(matches!(
        discover_instruction_sources(
            &LocalInstructionSourceReader,
            None,
            invalid.path(),
            invalid.path()
        ),
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

/// Discovery reads only through its reader: a workspace that exists only
/// where the reader answers (here `/runner/...`) is discovered with the same
/// scopes and bytes as a local one, and the host filesystem is never asked.
#[test]
fn discovery_reads_the_workspace_only_through_its_reader() {
    use crate::test_readers::{RUNNER_ROOT, RemappedReader};

    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary.path().join("repo/a");
    fs::create_dir_all(&nested).unwrap();
    fs::write(
        temporary.path().join("repo").join(INSTRUCTION_FILE_NAME),
        "root",
    )
    .unwrap();
    fs::write(nested.join(INSTRUCTION_FILE_NAME), "a").unwrap();
    let reader = RemappedReader::new(temporary.path());
    let remote_root = Path::new(RUNNER_ROOT).join("repo");
    assert!(!remote_root.exists());

    let remote =
        discover_instruction_sources(&reader, None, &remote_root, &remote_root.join("a")).unwrap();
    let local = discover_instruction_sources(
        &LocalInstructionSourceReader,
        None,
        &temporary.path().join("repo"),
        &nested,
    )
    .unwrap();
    assert_eq!(remote, local);
    assert_eq!(
        remote
            .sources()
            .iter()
            .map(DiscoveredInstructionSource::content)
            .collect::<Vec<_>>(),
        ["root", "a"]
    );
    assert!(matches!(
        discover_instruction_sources(&reader, None, temporary.path(), temporary.path()),
        Err(InstructionDiscoveryError::Io { .. })
    ));
}

/// The product-global file is resolved and read through the reader too.
#[test]
fn the_product_home_is_read_through_the_reader() {
    use crate::test_readers::{RUNNER_ROOT, RemappedReader};

    let temporary = tempfile::tempdir().unwrap();
    fs::create_dir_all(temporary.path().join("home")).unwrap();
    fs::create_dir_all(temporary.path().join("repo")).unwrap();
    fs::write(
        temporary.path().join("home").join(INSTRUCTION_FILE_NAME),
        "global",
    )
    .unwrap();
    let reader = RemappedReader::new(temporary.path());
    let runner = Path::new(RUNNER_ROOT);
    let discovered = discover_instruction_sources(
        &reader,
        Some(&runner.join("home")),
        &runner.join("repo"),
        &runner.join("repo"),
    )
    .unwrap();
    assert_eq!(
        discovered
            .sources()
            .iter()
            .map(|source| (source.source_id(), source.content()))
            .collect::<Vec<_>>(),
        [("plak.agents.global.v1", "global")]
    );
    // A local home is not one the reader can see.
    let unseen = discover_instruction_sources(
        &reader,
        Some(&temporary.path().join("home")),
        &runner.join("repo"),
        &runner.join("repo"),
    )
    .unwrap();
    assert_eq!(unseen.sources(), [] as [DiscoveredInstructionSource; 0]);
}

/// Discovery validates every answer: an unavailable reader, an over-long
/// read, a chain of the wrong length and files over the aggregate budget all
/// fail; none is trusted, and the budget cannot underflow.
#[test]
fn a_misbehaving_reader_is_refused_not_trusted() {
    use crate::test_readers::{Misbehavior, ScriptedReader};

    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let root = temporary.path().join("repo");
    let nested = root.join("a");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&nested).unwrap();
    fs::write(home.join(INSTRUCTION_FILE_NAME), "global").unwrap();
    fs::write(root.join(INSTRUCTION_FILE_NAME), "root").unwrap();
    fs::write(nested.join(INSTRUCTION_FILE_NAME), "a").unwrap();
    let discover = |misbehavior, home: Option<&Path>| {
        discover_instruction_sources(&ScriptedReader(misbehavior), home, &root, &nested)
    };
    let invalid = InstructionDiscoveryError::Reader(SourceReadError::InvalidAnswer);

    assert_eq!(
        discover(Misbehavior::Unavailable, Some(&home)),
        Err(InstructionDiscoveryError::Reader(
            SourceReadError::Unavailable
        ))
    );
    assert_eq!(discover(Misbehavior::OverReads, None), Err(invalid.clone()));
    assert_eq!(
        discover(Misbehavior::OverReads, Some(&home)),
        Err(invalid.clone())
    );
    assert_eq!(
        discover(Misbehavior::DuplicatesEntries, None),
        Err(invalid.clone())
    );
    assert_eq!(discover(Misbehavior::DropsEntry, None), Err(invalid));
    assert_eq!(
        discover(Misbehavior::IgnoresBudget, None),
        Err(InstructionDiscoveryError::ContentTooLarge {
            max_bytes: MAX_DISCOVERED_CONTENT_BYTES
        })
    );
    // The global file spends the whole budget; the workspace chain is then
    // read with nothing left, and the total never exceeds the budget.
    let spent = discover(Misbehavior::IgnoresBudget, Some(&home)).unwrap();
    assert_eq!(
        spent
            .sources()
            .iter()
            .map(|source| source.content().len())
            .sum::<usize>(),
        MAX_DISCOVERED_CONTENT_BYTES
    );
}

/// A FIFO named `AGENTS.md`, in the workspace or the product home, is refused
/// as a non-regular file without blocking on its open.
#[test]
fn a_fifo_instruction_file_is_refused_without_blocking() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let root = temporary.path().join("repo");
    let other = temporary.path().join("other");
    for directory in [&home, &root, &other] {
        fs::create_dir_all(directory).unwrap();
    }
    for fifo in [
        root.join(INSTRUCTION_FILE_NAME),
        home.join(INSTRUCTION_FILE_NAME),
    ] {
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let (home_thread, root_thread, other_thread) = (home.clone(), root.clone(), other.clone());
    std::thread::spawn(move || {
        let in_workspace = discover_instruction_sources(
            &LocalInstructionSourceReader,
            None,
            &root_thread,
            &root_thread,
        );
        let in_home = discover_instruction_sources(
            &LocalInstructionSourceReader,
            Some(&home_thread),
            &other_thread,
            &other_thread,
        );
        let _ = sender.send((in_workspace, in_home));
    });
    // A liveness bound only: generous, so load never trips it.
    let (in_workspace, in_home) = receiver
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("discovery blocked on a FIFO open");
    assert!(matches!(
        in_workspace,
        Err(InstructionDiscoveryError::NotRegularFile { .. })
    ));
    assert!(matches!(
        in_home,
        Err(InstructionDiscoveryError::NotRegularFile { .. })
    ));
}
