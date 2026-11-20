use std::fs;
use std::os::unix::fs::symlink;

use serde_json::{Value, json};

use super::*;

fn manifest(platform: &str, files: BTreeMap<String, FileFacts>) -> Manifest {
    let mut build_info = current().unwrap();
    build_info.flavor = "maintainer release".into();
    build_info.revision = "a".repeat(40);
    build_info.omitted_features.clear();
    build_info.test_hooks = false;
    Manifest {
        snapshot_schema: 1,
        snapshot_id: shipping::snapshot_id(&files),
        platform: platform.into(),
        files,
        build_info,
        glibc: (platform == GNU).then_some([2, 39, 0]),
    }
}

fn files() -> BTreeMap<String, FileFacts> {
    NAMES
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                FileFacts {
                    length: 3,
                    sha256: "a".repeat(64),
                    display_id: "a".repeat(12),
                },
            )
        })
        .collect()
}

#[test]
fn maintainer_manifest_rejects_impostor_builds_and_noncanonical_pins() {
    let valid = manifest(MUSL, files());
    let canonical = shipping::canonical(&valid).unwrap();
    Manifest::parse(&canonical).unwrap();
    let mut padded = canonical.clone();
    padded.insert(0, b' ');
    assert!(
        Manifest::parse(&padded)
            .unwrap_err()
            .message()
            .contains("noncanonical")
    );
    let mut extra = serde_json::to_value(&valid).unwrap();
    extra["future"] = json!(true);
    assert!(
        Manifest::parse(&shipping::canonical(&extra).unwrap())
            .unwrap_err()
            .message()
            .contains("invalid maintainer snapshot manifest")
    );
    for (where_, bad, error) in [
        ("/build_info/test_hooks", json!(true), "build or platform"),
        (
            "/build_info/flavor",
            json!("maintainer debug"),
            "build or platform",
        ),
        (
            "/build_info/omitted_features",
            json!(["openai"]),
            "build or platform",
        ),
        (
            "/build_info/product_version",
            json!("9.9.9"),
            "build or platform",
        ),
        ("/snapshot_id", json!("b".repeat(64)), "snapshot id differs"),
        ("/glibc", json!([2, 39, 0]), "build or platform"),
    ] {
        let mut changed = serde_json::to_value(&valid).unwrap();
        *changed.pointer_mut(where_).unwrap() = bad;
        let decoded: Manifest = serde_json::from_value(changed).unwrap();
        assert!(
            Manifest::parse(&shipping::canonical(&decoded).unwrap())
                .unwrap_err()
                .message()
                .contains(error),
            "{where_}"
        );
    }
}

#[test]
fn maintainer_snapshot_rejects_noncanonical_sidecar_and_wrong_state_marker() {
    let lib = tempfile::tempdir().unwrap();
    let payload = lib.path().join("payload");
    fs::write(&payload, "exe").unwrap();
    let fact = FileFacts::read(&payload).unwrap();
    let manifest = manifest(
        MAC,
        NAMES
            .into_iter()
            .map(|name| (name.to_owned(), fact.clone()))
            .collect(),
    );
    let root = lib.path().join("maintainer").join(&manifest.snapshot_id);
    fs::create_dir_all(&root).unwrap();
    fs::copy(&payload, root.join("plakd")).unwrap();
    for name in &NAMES[1..] {
        let dir = lib.path().join("sidecars").join(name).join(&fact.sha256);
        fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join(name);
        fs::copy(&payload, &sidecar).unwrap();
        symlink(sidecar, root.join(name)).unwrap();
    }
    let marker = root.join("state-key");
    fs::write(&marker, format!("{}\n", manifest.build_info.state_key)).unwrap();
    assert_eq!(manifest.inputs(&root).unwrap().len(), 3);
    fs::write(&marker, "wrong\n").unwrap();
    assert!(
        manifest
            .inputs(&root)
            .unwrap_err()
            .message()
            .contains("state-key marker differs")
    );
    fs::write(marker, format!("{}\n", manifest.build_info.state_key)).unwrap();
    let worker = root.join(NAMES[1]);
    fs::remove_file(&worker).unwrap();
    symlink(&payload, &worker).unwrap(); // Identical bytes, but outside the published sidecar path.
    assert!(
        manifest
            .inputs(&root)
            .unwrap_err()
            .message()
            .contains("sidecar link is not canonical")
    );
}

#[test]
fn maintainer_target_checks_running_revision_and_state_key() {
    let mut pinned = manifest(MAC, files());
    for field in [
        "revision",
        "state_key",
        "omitted_features",
        "product_version",
    ] {
        let mut bad: Value = serde_json::to_value(&pinned.build_info).unwrap();
        bad[field] = match field {
            "revision" => json!("c".repeat(40)),
            "omitted_features" => json!(["openai"]),
            "product_version" => json!("9.9.9"),
            _ => json!("different"),
        };
        let bad = serde_json::from_value(bad).unwrap();
        assert!(
            check_bootstrap_pin(
                &bad,
                MAC,
                MAC,
                &pinned.build_info.revision,
                &pinned.build_info.state_key
            )
            .is_err(),
            "{field}"
        );
        assert!(
            pinned
                .check_running(&bad, MAC, Path::new("/unused"))
                .is_err(),
            "{field}"
        );
        if matches!(field, "omitted_features" | "product_version") {
            let valid = std::mem::replace(&mut pinned.build_info, bad);
            assert!(
                check_bootstrap_pin(
                    &pinned.build_info,
                    MAC,
                    MAC,
                    &pinned.build_info.revision,
                    &pinned.build_info.state_key
                )
                .is_err()
            );
            assert!(
                pinned
                    .check_running(&pinned.build_info, MAC, Path::new("/unused"))
                    .is_err()
            );
            pinned.build_info = valid;
        }
    }
    check_bootstrap_pin(
        &pinned.build_info,
        MAC,
        MAC,
        &pinned.build_info.revision,
        &pinned.build_info.state_key,
    )
    .unwrap();
    pinned
        .check_running(&pinned.build_info, MAC, Path::new("/unused"))
        .unwrap();
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn maintainer_target_remeasures_glibc_from_the_elf_files() {
    let root = tempfile::tempdir().unwrap();
    for name in NAMES {
        symlink("/bin/sh", root.path().join(name)).unwrap();
    }
    let measured = glibc_ceiling(root.path()).unwrap();
    let mut pinned = manifest(GNU, files());
    pinned.glibc = Some(measured);
    pinned
        .check_running(&pinned.build_info, GNU, root.path())
        .unwrap();
    pinned.glibc = Some([measured[0], measured[1] + 1, measured[2]]);
    assert!(
        pinned
            .check_running(&pinned.build_info, GNU, root.path())
            .unwrap_err()
            .message()
            .contains("transferred GLIBC ceiling differs")
    );
}

#[test]
fn glibc_requirements_are_compared_numerically_and_fail_closed() {
    let raw = "  0x0010: Name: GLIBC_2.9 Flags: none\n  0x0020: Name: GLIBC_2.34 Flags: none\n";
    assert_eq!(required_glibc(raw).unwrap(), [2, 34, 0]);
    assert!(required_glibc("Name: GLIBC_PRIVATE").is_err());
    assert!(required_glibc("no version information").is_err());
    assert!(glibc_version("2.34.x").is_err());
    assert_eq!(glibc_version("2.2.5").unwrap(), [2, 2, 5]);
}
