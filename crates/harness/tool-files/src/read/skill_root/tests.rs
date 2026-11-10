use super::*;
use std::fs;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temporary.path()).unwrap().join("skills");
    fs::create_dir_all(root.join("deploy/scripts")).unwrap();
    fs::write(
        root.join("deploy/SKILL.md"),
        "---\nname: deploy\n---\nbody\n",
    )
    .unwrap();
    (temporary, root)
}

#[test]
fn only_absolute_paths_strictly_under_a_root_are_admitted() {
    let (_temporary, root) = fixture();
    let roots = vec![root.clone()];
    let file = root.join("deploy/SKILL.md");
    assert_eq!(containing_root(&roots, &file), Some(root.as_path()));
    assert_eq!(containing_root(&roots, &root), None);
    assert_eq!(containing_root(&roots, Path::new("deploy/SKILL.md")), None);
    assert_eq!(
        containing_root(&roots, &root.join("deploy/../deploy/SKILL.md")),
        None
    );
    assert_eq!(containing_root(&roots, Path::new("/etc/passwd")), None);
    assert_eq!(containing_root(&[], &file), None);
}

#[test]
fn a_regular_file_under_the_root_reads_to_eof() {
    let (_temporary, root) = fixture();
    let (bytes, complete) = read_leading(&root, &root.join("deploy/SKILL.md"), 10).unwrap();
    assert_eq!(bytes, b"---\nname: deploy\n---\nbody\n");
    assert!(complete);
}

#[test]
fn slice_identity_pins_inode_size_and_modification_time() {
    let (_temporary, root) = fixture();
    let path = root.join("deploy/SKILL.md");
    let (_, _, first) = read_slice(&root, &path, 0).unwrap();
    assert_eq!(read_slice(&root, &path, 0).unwrap().2, first);
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_times(
        fs::FileTimes::new()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(123)),
    )
    .unwrap();
    let modified = read_slice(&root, &path, 0).unwrap().2;
    assert_ne!(modified, first);
    assert_eq!(modified.inode, first.inode);
    assert_eq!(modified.size, first.size);
    fs::write(&path, "longer replacement contents for this skill\n").unwrap();
    let resized = read_slice(&root, &path, 0).unwrap().2;
    assert_ne!(resized.size, modified.size);
    let replacement = root.join("replacement");
    fs::write(&replacement, "new inode\n").unwrap();
    fs::rename(replacement, &path).unwrap();
    let replaced = read_slice(&root, &path, 0).unwrap().2;
    assert_ne!(replaced.inode, resized.inode);
}

#[test]
fn missing_paths_directories_and_escaping_links_are_refused_without_effect() {
    let (temporary, root) = fixture();
    let outside = temporary.path().join("secret.txt");
    fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("deploy/link.txt")).unwrap();
    fs::write(root.join("deploy/scripts/run.sh"), "echo ok\n").unwrap();
    std::os::unix::fs::symlink(
        root.join("deploy/scripts/run.sh"),
        root.join("deploy/inside.sh"),
    )
    .unwrap();

    let missing = read_leading(&root, &root.join("deploy/missing.md"), 1).unwrap_err();
    assert_eq!(missing.kind(), ToolRuntimeErrorKind::WorkspacePolicy);
    assert!(
        missing
            .message()
            .as_str()
            .starts_with("path does not exist: ")
    );
    assert!(missing.proven_no_effect());

    let directory = read_leading(&root, &root.join("deploy/scripts"), 1).unwrap_err();
    assert!(
        directory
            .message()
            .as_str()
            .starts_with("path is a directory: ")
    );

    let escaped = read_leading(&root, &root.join("deploy/link.txt"), 1).unwrap_err();
    assert!(
        escaped
            .message()
            .as_str()
            .starts_with("path resolves outside the skills directory: ")
    );

    let (bytes, _) = read_leading(&root, &root.join("deploy/inside.sh"), 1).unwrap();
    assert_eq!(bytes, b"echo ok\n");
}

#[test]
fn a_fifo_under_the_root_is_refused_without_blocking() {
    let (_temporary, root) = fixture();
    let pipe = root.join("deploy/pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .unwrap();
    assert!(status.success());
    let refused = read_leading(&root, &pipe, 1).unwrap_err();
    assert_eq!(refused.kind(), ToolRuntimeErrorKind::WorkspacePolicy);
    assert!(
        refused
            .message()
            .as_str()
            .starts_with("path is not a regular file: ")
    );
}
