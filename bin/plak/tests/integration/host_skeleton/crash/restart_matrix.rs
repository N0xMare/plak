//! Run-local twin pilots and disjoint, independently scheduled restart cuts.

use super::client_control::{Commit, Signature};
use super::*;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::MetadataExt as _;

const GROUPS: usize = 8;

pub(super) struct Pilot {
    pub(super) commits: Vec<Commit>,
    pub(super) expected: Signature,
}

fn fingerprint(path: &Path) -> serde_json::Value {
    let metadata = std::fs::metadata(path).unwrap();
    json!({
        "path": path.canonicalize().unwrap(), "length": metadata.len(),
        "device": metadata.dev(), "inode": metadata.ino(),
        "modified": [metadata.mtime(), metadata.mtime_nsec()],
        "changed": [metadata.ctime(), metadata.ctime_nsec()],
    })
}

fn digest(bytes: &[u8]) -> String {
    plakous::model::sha256_hex(&[bytes])
}

fn lock(path: &Path) -> File {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.lock().unwrap();
    file
}

fn publish(path: &Path, bytes: &[u8]) {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes).unwrap();
    std::fs::rename(temporary, path).unwrap();
}

fn run_identity(nextest: Option<&str>, run: Option<&str>, process: u32, started: u128) -> String {
    match (nextest, run) {
        (Some("1"), Some(run)) => run.to_owned(),
        _ => format!("process-{process}-{started}"),
    }
}

#[test]
fn inherited_nextest_run_id_does_not_share_cargo_test_pilots() {
    for marker in [None, Some("0"), Some("true")] {
        let first = run_identity(marker, Some("inherited"), 10, 100);
        assert_eq!(first, "process-10-100");
        assert_ne!(first, run_identity(marker, Some("inherited"), 11, 100));
        assert_ne!(first, run_identity(marker, Some("inherited"), 10, 101));
    }
}

#[test]
fn nextest_siblings_share_only_the_current_run() {
    let first = run_identity(Some("1"), Some("current-run"), 10, 100);
    assert_eq!(first, run_identity(Some("1"), Some("current-run"), 11, 101));
    assert_ne!(first, run_identity(Some("1"), Some("another-run"), 10, 100));
    assert_eq!(run_identity(Some("1"), None, 10, 100), "process-10-100");
}

fn cache_root() -> PathBuf {
    static RUN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let run = RUN.get_or_init(|| {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        run_identity(
            std::env::var("NEXTEST").ok().as_deref(),
            std::env::var("NEXTEST_RUN_ID").ok().as_deref(),
            std::process::id(),
            started,
        )
    });
    let root = std::env::temp_dir().join("restart-matrices").join(run);
    std::fs::create_dir_all(&root).unwrap();
    let test = std::env::current_exe().unwrap();
    let product = Path::new(env!("CARGO_BIN_EXE_plak"));
    let environment: BTreeMap<_, _> = std::env::vars_os()
        .filter(|(key, _)| {
            key.to_str().is_some_and(|key| {
                key.starts_with("PLAK_")
                    || matches!(
                        key,
                        "PATH" | "HOME" | "TERM" | "GIT_CONFIG_GLOBAL" | "GIT_CONFIG_NOSYSTEM"
                    )
            })
        })
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.as_encoded_bytes().to_vec(),
            )
        })
        .collect();
    let inputs = json!({"test": fingerprint(&test), "product": fingerprint(product),
        "environment": digest(&serde_json::to_vec(&environment).unwrap()), "groups": GROUPS});
    let identity = digest(&serde_json::to_vec(&inputs).unwrap());
    let _lock = lock(&root.join(format!("build-{identity}.lock")));
    let identity_file = root.join(format!("build-{identity}.json"));
    // Hash both exact executables once per run and input identity, not once
    // per nextest process. Changed paths or file identities cannot reuse it.
    let build: String = if identity_file.exists() {
        serde_json::from_slice(&std::fs::read(identity_file).unwrap()).unwrap()
    } else {
        let build = digest(
            &serde_json::to_vec(&json!({"inputs": inputs,
                "test": digest(&std::fs::read(&test).unwrap()),
                "product": digest(&std::fs::read(product).unwrap()),
            }))
            .unwrap(),
        );
        assert_eq!(
            inputs["test"],
            fingerprint(&test),
            "test executable changed during pilot setup"
        );
        assert_eq!(
            inputs["product"],
            fingerprint(product),
            "product executable changed during pilot setup"
        );
        publish(&identity_file, &serde_json::to_vec(&build).unwrap());
        build
    };
    let root = root.join(build);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn shared_pilot(root: &Path, shape: &str, child: bool, prepare: impl FnOnce() -> Pilot) -> Pilot {
    let path = root.join(format!("{shape}-{child}-pilot.json"));
    let mut lock = lock(&path.with_extension("lock"));
    if path.exists() {
        let (commits, expected) = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        return Pilot { commits, expected };
    }
    // A failed or killed builder poisons this run's pilot instead of making
    // every remaining test retry an expensive, already-failing setup.
    assert_eq!(
        lock.metadata().unwrap().len(),
        0,
        "the shared pilot did not finish"
    );
    lock.write_all(b"started\n").unwrap();
    let pilot = prepare();
    publish(
        &path,
        &serde_json::to_vec(&(&pilot.commits, &pilot.expected)).unwrap(),
    );
    pilot
}

pub(super) fn run_group(
    shape: &str,
    child: bool,
    group: usize,
    prepare: impl FnOnce() -> Pilot,
    mut cut: impl FnMut(&str, u64, &Signature) -> serde_json::Value,
) {
    assert!(group < GROUPS);
    let root = cache_root();
    let Pilot {
        mut commits,
        expected,
    } = shared_pilot(&root, shape, child, prepare);
    commits.sort_by(|left, right| (&left.1, left.2).cmp(&(&right.1, right.2)));
    let inventory: Vec<_> = commits
        .iter()
        .map(|(_, store, ordinal)| (store.clone(), *ordinal))
        .collect();
    assert!(
        inventory.windows(2).all(|pair| pair[0] != pair[1]),
        "pilot cuts must be unique"
    );
    let mut partition: Vec<_> = (0..GROUPS)
        .flat_map(|group| commits.iter().skip(group).step_by(GROUPS))
        .map(|(_, store, ordinal)| (store.clone(), *ordinal))
        .collect();
    partition.sort();
    assert_eq!(
        partition, inventory,
        "the eight groups must partition the complete pilot inventory"
    );
    let selected: Vec<_> = commits.iter().skip(group).step_by(GROUPS).collect();
    assert!(!selected.is_empty(), "restart cut group must not be empty");
    let evidence = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/crash-sweep")
        .join(format!(
            "{}-{}-{group}-{}.jsonl",
            shape.replace('_', "-"),
            if child { "child" } else { "root" },
            std::process::id()
        ));
    std::fs::create_dir_all(evidence.parent().unwrap()).unwrap();
    let mut table = String::new();
    let mut completed = Vec::new();
    for (_, store, ordinal) in &selected {
        let row = cut(store, *ordinal, &expected);
        assert_eq!(row["shape"], shape);
        assert_eq!(row["child"], child);
        let reached = (
            row["store"].as_str().unwrap().to_owned(),
            row["cut"].as_u64().unwrap(),
        );
        assert_eq!((&reached.0, reached.1), (store, *ordinal));
        completed.push(reached);
        writeln!(table, "{row}").unwrap();
        std::fs::write(&evidence, &table).unwrap();
    }
    publish(
        &root.join(format!("{shape}-{child}-group-{group}.json")),
        &serde_json::to_vec(&completed).unwrap(),
    );
    let manifests: Vec<_> = (0..GROUPS)
        .map(|group| root.join(format!("{shape}-{child}-group-{group}.json")))
        .collect();
    if manifests.iter().all(|path| path.exists()) {
        let mut reached: Vec<(String, u64)> = manifests
            .iter()
            .flat_map(|path| {
                serde_json::from_slice::<Vec<(String, u64)>>(&std::fs::read(path).unwrap()).unwrap()
            })
            .collect();
        reached.sort();
        assert_eq!(
            reached, inventory,
            "the eight groups must run every pilot cut exactly once"
        );
    }
    eprintln!(
        "{shape}: child={child} group={group}: {} cuts; {}",
        selected.len(),
        evidence.display()
    );
}

macro_rules! matrix_tests {
    ($run:expr) => {
        #[test]
        fn root_0() {
            ($run)(false, 0);
        }
        #[test]
        fn root_1() {
            ($run)(false, 1);
        }
        #[test]
        fn root_2() {
            ($run)(false, 2);
        }
        #[test]
        fn root_3() {
            ($run)(false, 3);
        }
        #[test]
        fn root_4() {
            ($run)(false, 4);
        }
        #[test]
        fn root_5() {
            ($run)(false, 5);
        }
        #[test]
        fn root_6() {
            ($run)(false, 6);
        }
        #[test]
        fn root_7() {
            ($run)(false, 7);
        }
        #[test]
        fn child_0() {
            ($run)(true, 0);
        }
        #[test]
        fn child_1() {
            ($run)(true, 1);
        }
        #[test]
        fn child_2() {
            ($run)(true, 2);
        }
        #[test]
        fn child_3() {
            ($run)(true, 3);
        }
        #[test]
        fn child_4() {
            ($run)(true, 4);
        }
        #[test]
        fn child_5() {
            ($run)(true, 5);
        }
        #[test]
        fn child_6() {
            ($run)(true, 6);
        }
        #[test]
        fn child_7() {
            ($run)(true, 7);
        }
    };
}

pub(super) use matrix_tests;
