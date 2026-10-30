use std::fs;

use tempfile::tempdir;

use super::*;

/// The refusal a reader acts on states the rule and the exception.
///
/// An outside path and a traversing path are different mistakes and must
/// not share a sentence, and neither may claim that absolute paths are
/// refused, because an absolute path under the root is accepted (spec 04
/// §2.1). The shell's freedom from the same rule (§2.2) is stated rather
/// than left to be discovered.
#[test]
fn a_path_refusal_states_the_rule_and_where_the_shell_differs()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempdir()?;
    fs::write(workspace.path().join("inside.txt"), b"inside")?;
    let policy = WorkspacePolicy::host(workspace.path(), ".")?;
    let canonical = workspace.path().canonicalize()?;

    let outside = policy
        .relative_input("/etc/passwd")
        .expect_err("an absolute path outside the root is refused");
    assert_eq!(
        outside.message().as_str(),
        format!(
            "path is outside the workspace root `{}`; file tools accept a path inside the workspace root: a relative path with no `..`, or an absolute path under the root. a command the shell tool runs is not bound by this rule and can reach outside the root",
            canonical.display()
        )
    );

    let traversing = policy
        .relative_input("../outside")
        .expect_err("a traversing path is refused");
    assert_eq!(
        traversing.message().as_str(),
        "path leaves the workspace root with `..`; file tools accept a path inside the workspace root: a relative path with no `..`, or an absolute path under the root. a command the shell tool runs is not bound by this rule and can reach outside the root"
    );

    // The rule the message states is the rule the policy applies.
    assert_eq!(
        policy.relative_input(canonical.join("inside.txt"))?,
        PathBuf::from("inside.txt")
    );
    Ok(())
}

#[test]
fn workspace_rejects_traversal_and_symlink_escape() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempdir()?;
    let outside = tempdir()?;
    fs::write(workspace.path().join("inside.txt"), b"inside")?;
    let policy = WorkspacePolicy::host(workspace.path(), ".")?;
    // Resolution canonicalizes (macOS `/var` -> `/private/var`), so compare
    // against the canonical root rather than the raw tempdir path.
    assert!(
        policy
            .resolve_existing("inside.txt")?
            .starts_with(workspace.path().canonicalize()?)
    );
    assert!(policy.resolve_existing("../outside").is_err());

    let canonical = workspace.path().canonicalize()?;
    let under_root = canonical.join("inside.txt");
    assert!(
        policy
            .resolve_existing(&under_root)?
            .starts_with(&canonical)
    );
    assert_eq!(policy.relative_input(&canonical)?, PathBuf::from("."));
    assert_eq!(
        policy.relative_input(canonical.join("inside.txt"))?,
        PathBuf::from("inside.txt")
    );
    assert!(policy.relative_input("/etc/passwd").is_err());
    assert!(
        policy
            .relative_input(canonical.join("..").join("outside"))
            .is_err()
    );
    let mut root_slash = canonical.clone().into_os_string();
    root_slash.push("/");
    assert_eq!(policy.relative_input(root_slash)?, PathBuf::from("."));
    if let Some(parent) = canonical.parent() {
        let false_prefix = parent.join(format!(
            "{}-extra",
            canonical
                .file_name()
                .expect("canonical workspace root has a file name")
                .to_string_lossy()
        ));
        assert!(policy.relative_input(false_prefix.join("foo")).is_err());
    }

    {
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("escape"))?;
        assert!(policy.resolve_existing("escape").is_err());
    }
    Ok(())
}

#[test]
fn workspace_paths_resolve_from_root_not_cwd() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempdir()?;
    fs::write(workspace.path().join("Cargo.toml"), b"[package]\n")?;
    fs::create_dir(workspace.path().join("src"))?;
    fs::write(workspace.path().join("src/lib.rs"), b"pub fn lib() {}\n")?;
    let policy = WorkspacePolicy::host(workspace.path(), "src")?;
    assert!(
        policy
            .resolve_existing("Cargo.toml")?
            .ends_with("Cargo.toml")
    );
    assert!(policy.resolve_existing("lib.rs").is_err());
    assert!(
        policy
            .resolve_existing("src/lib.rs")?
            .ends_with("src/lib.rs")
    );
    Ok(())
}

#[test]
fn generation_stamp_is_live_and_omitted_by_default() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempdir()?;
    let policy = WorkspacePolicy::host(workspace.path(), ".")?;
    assert_eq!(policy.generation_stamp(), None);
    let counter = Arc::new(AtomicU64::new(3));
    let stamped =
        WorkspacePolicy::host(workspace.path(), ".")?.with_generation_stamp(counter.clone());
    assert_eq!(stamped.generation_stamp(), Some(3));
    counter.store(4, Ordering::SeqCst);
    assert_eq!(stamped.generation_stamp(), Some(4));
    Ok(())
}
