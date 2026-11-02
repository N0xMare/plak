use std::path::Path;

use super::{LocalWorkspaceProbe, WorkspaceProbe};

#[test]
fn local_probe_resolves_and_inspects_without_following_links_for_size() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    std::fs::write(root.join("a"), b"12345").unwrap();
    std::fs::write(root.join("nested/b"), b"123").unwrap();
    let outside = directory.path().join("outside");
    std::fs::write(&outside, vec![0_u8; 4096]).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

    let probe = LocalWorkspaceProbe;
    let canonical = probe.canonicalize(&root.join("nested/..")).unwrap();
    assert_eq!(canonical, Some(std::fs::canonicalize(&root).unwrap()));
    assert_eq!(probe.canonicalize(&root.join("missing")), Ok(None));

    assert_eq!(probe.is_file(&root.join("a")), Ok(true));
    assert_eq!(probe.is_file(&root.join("link")), Ok(true));
    assert_eq!(probe.is_file(&root.join("nested")), Ok(false));
    assert_eq!(probe.is_file(&root.join("missing")), Ok(false));

    let entry = |path: &Path| std::fs::symlink_metadata(path).unwrap().len();
    let expected = entry(&root)
        + entry(&root.join("a"))
        + entry(&root.join("nested"))
        + entry(&root.join("nested/b"))
        + entry(&root.join("link"));
    assert_eq!(probe.tree_bytes(&root), Ok(Some(expected)));
    assert_eq!(probe.tree_bytes(&root.join("a")), Ok(Some(5)));
    assert_eq!(probe.tree_bytes(&root.join("missing")), Ok(None));
}
