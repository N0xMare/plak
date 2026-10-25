use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Commit {
    pub ordinal: u64,
    pub store: String,
    pub store_index: u64,
    pub store_ordinal: u64,
    pub label: String,
    pub tables: Vec<String>,
    pub indexed: bool,
}

impl Commit {
    pub fn signature(&self) -> String {
        format!("{}:{}:{:?}", self.store, self.label, self.tables)
    }
}

/// Only indexed commits advance a lifecycle; lease renewals are not progress.
pub(super) fn progress(path: &Path) -> u64 {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    body.rsplit_once('\n')
        .and_then(|(complete, _)| complete.lines().next_back())
        .and_then(|line| serde_json::from_str::<Commit>(line).ok())
        .map_or(0, |commit| commit.ordinal)
}

#[test]
fn lease_renewals_and_partial_rows_do_not_advance_lifecycle_progress() {
    let directory = tempfile::tempdir().expect("trace directory");
    let path = directory.path().join("trace.jsonl");
    assert_eq!(progress(&path), 0);
    let mut commit = Commit {
        ordinal: 1,
        store: "runtime".into(),
        store_index: 1,
        store_ordinal: 1,
        label: "provider_outcome".into(),
        tables: Vec::new(),
        indexed: true,
    };
    let mut body = format!("{}\n", serde_json::to_string(&commit).expect("commit"));
    std::fs::write(&path, &body).expect("trace");
    assert_eq!(progress(&path), 1);
    commit.indexed = false;
    commit.label = "renew".into();
    for index in 2..5 {
        commit.store_index = index;
        body.push_str(&serde_json::to_string(&commit).expect("renew"));
        body.push('\n');
        std::fs::write(&path, &body).expect("trace");
        assert_eq!(progress(&path), 1);
    }
    body.push_str("{\"ordinal\":2,");
    std::fs::write(&path, &body).expect("partial row");
    assert_eq!(progress(&path), 1);
    commit.ordinal = 2;
    commit.indexed = true;
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&commit).expect("commit")),
    )
    .expect("trace");
    assert_eq!(progress(&path), 2);
}

pub(super) fn read(path: &Path) -> Vec<Commit> {
    let body = std::fs::read_to_string(path).expect("commit trace");
    assert!(body.ends_with('\n'), "trace ended with an incomplete row");
    let commits: Vec<Commit> = body
        .lines()
        .map(|line| serde_json::from_str(line).expect("trace row"))
        .collect();
    let mut ordinal = 0;
    let mut stores = std::collections::BTreeMap::new();
    for commit in &commits {
        if commit.indexed {
            ordinal += 1;
        }
        assert_eq!(commit.ordinal, ordinal, "non-contiguous trace ordinal");
        let (index, indexed) = stores.entry(&commit.store).or_insert((0, 0));
        *index += 1;
        if commit.indexed {
            *indexed += 1;
        }
        assert_eq!(commit.store_index, *index, "non-contiguous store index");
        assert_eq!(
            commit.store_ordinal, *indexed,
            "non-contiguous indexed store ordinal"
        );
        assert!(commit.tables.windows(2).all(|pair| pair[0] < pair[1]));
    }
    commits
        .into_iter()
        .filter(|commit| commit.indexed)
        .collect()
}

pub(super) fn sequences(commits: &[Commit]) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut stores = std::collections::BTreeMap::<String, Vec<String>>::new();
    for commit in commits {
        stores
            .entry(commit.store.clone())
            .or_default()
            .push(commit.signature());
    }
    stores
}

pub(super) fn valid_cut(pilot: &[Commit], actual: &[Commit], target: &Commit) -> bool {
    let expected = sequences(pilot);
    let seen = sequences(actual);
    let Some(last) = actual.last() else {
        return false;
    };
    last.store == target.store
        && last.store_ordinal == target.store_ordinal
        && seen.iter().all(|(store, sequence)| {
            expected
                .get(store)
                .is_some_and(|prefix| prefix.starts_with(sequence))
        })
        && seen
            .get(&target.store)
            .is_some_and(|sequence| sequence.len() as u64 == target.store_ordinal)
}

#[test]
fn cross_store_interleaving_is_free_but_each_store_prefix_is_exact() {
    let commit = |store: &str, ordinal: u64, label: &str| Commit {
        ordinal,
        store: store.to_owned(),
        store_index: 1,
        store_ordinal: 1,
        label: label.to_owned(),
        tables: Vec::new(),
        indexed: true,
    };
    let runtime = commit("runtime", 1, "commit");
    let product = commit("product", 2, "unlabelled");
    let pilot = vec![runtime.clone(), product.clone()];
    assert!(valid_cut(
        &pilot,
        &[product.clone(), runtime.clone()],
        &runtime
    ));
    assert!(valid_cut(&pilot, std::slice::from_ref(&runtime), &runtime));
    assert!(!valid_cut(
        &pilot,
        &[product, commit("runtime", 2, "fork")],
        &runtime
    ));
}

#[test]
fn unlabelled_table_changes_diverge_between_twin_pilots() {
    let first = Commit {
        ordinal: 1,
        store: "product".to_owned(),
        store_index: 1,
        store_ordinal: 1,
        label: "unlabelled".to_owned(),
        tables: vec!["sessions".to_owned()],
        indexed: true,
    };
    let mut second = first.clone();
    second.tables = vec!["submissions".to_owned()];
    assert_ne!(sequences(&[first]), sequences(&[second]));
}

#[test]
fn a_cut_checks_each_store_tables_without_requiring_equal_progress() {
    let target = Commit {
        ordinal: 1,
        store: "runtime".to_owned(),
        store_index: 1,
        store_ordinal: 1,
        label: "commit".to_owned(),
        tables: Vec::new(),
        indexed: true,
    };
    let mut unrelated = target.clone();
    unrelated.store = "product".to_owned();
    unrelated.tables = vec!["sessions".to_owned()];
    assert!(!valid_cut(
        std::slice::from_ref(&target),
        &[unrelated, target.clone()],
        &target
    ));
    let mut later = target.clone();
    later.store = "product".to_owned();
    let longer_pilot = vec![target.clone(), later];
    assert!(valid_cut(
        &longer_pilot,
        std::slice::from_ref(&target),
        &target
    ));
}
