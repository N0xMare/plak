use super::*;
use std::os::unix::fs::{PermissionsExt as _, symlink};

const IMAGE_ID: &str = "6105d6cc76af";

fn snapshot(root: &Path, name: &str) -> PathBuf {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let image = directory.join("plak");
    std::fs::write(&image, "image").unwrap();
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o755)).unwrap();
    manifest(&image, IMAGE_ID);
    image
}

fn manifest(image: &Path, binary: &str) {
    let fields = crate::build_identity::image_fields(&image.metadata().unwrap());
    std::fs::write(
        image.with_file_name("binary-id"),
        format!("{binary} {fields}\n"),
    )
    .unwrap();
}

#[test]
fn runner_binary_selects_only_a_matching_installed_image_across_both_flavors() {
    let home = tempfile::tempdir().unwrap();
    let roots =
        ["maintainer", "release"].map(|flavor| home.path().join(".local/lib/plak").join(flavor));
    for root in &roots {
        let image = snapshot(root, &"a".repeat(64));
        assert_eq!(select(std::slice::from_ref(root), IMAGE_ID), Some(image));
    }
    assert!(select(&roots, IMAGE_ID).is_some());
    for invalid in ["", "../plak", "ABCDEF012345", "6105", "000000000000"] {
        assert!(select(&roots, invalid).is_none());
    }
    assert!(select(&[home.path().join("missing")], IMAGE_ID).is_none());
}

#[test]
fn runner_binary_refuses_unsafe_paths_manifests_and_mismatched_image_bytes() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join(".local/lib/plak/release");
    let image = snapshot(&root, &"a".repeat(64));
    let directory = image.parent().unwrap();
    let kept = root.join("kept");
    std::fs::rename(directory, &kept).unwrap();
    symlink(&kept, directory).unwrap();
    assert!(select(std::slice::from_ref(&root), IMAGE_ID).is_none());
    std::fs::remove_file(directory).unwrap();
    std::fs::rename(kept, directory).unwrap();
    for path in [&image, directory, root.as_path(), root.parent().unwrap()] {
        let original = path.metadata().unwrap().permissions();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(select(std::slice::from_ref(&root), IMAGE_ID).is_none());
        std::fs::set_permissions(path, original).unwrap();
    }
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(select(std::slice::from_ref(&root), IMAGE_ID).is_none());
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&image, "different").unwrap();
    manifest(&image, IMAGE_ID);
    assert!(select(std::slice::from_ref(&root), IMAGE_ID).is_none());
    std::fs::write(&image, "image").unwrap();
    manifest(&image, IMAGE_ID);
    let saved = image.with_file_name("saved");
    std::fs::rename(&image, &saved).unwrap();
    symlink(&saved, &image).unwrap();
    assert!(select(std::slice::from_ref(&root), IMAGE_ID).is_none());
    std::fs::remove_file(&image).unwrap();
    std::fs::rename(saved, &image).unwrap();
    let manifest = image.with_file_name("binary-id");
    std::fs::remove_file(&manifest).unwrap();
    symlink(&image, &manifest).unwrap();
    assert!(select(std::slice::from_ref(&root), IMAGE_ID).is_none());
    assert!(select(&[home.path().to_owned()], IMAGE_ID).is_none());
}
