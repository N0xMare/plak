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
    pub classes: Vec<String>,
    pub retired: u64,
}

impl Commit {
    pub fn signature(&self) -> String {
        format!("{}:{}:{:?}", self.store, self.label, self.tables)
    }
}

/// Child and parent connections may interleave under the same store kind.
/// Coverage must agree between twins, not metadata at each merged ordinal.
pub(super) fn metadata_coverage(
    commits: &[Commit],
) -> std::collections::BTreeMap<
    String,
    (
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<u64>,
    ),
> {
    let mut coverage = std::collections::BTreeMap::<
        String,
        (
            std::collections::BTreeSet<String>,
            std::collections::BTreeSet<u64>,
        ),
    >::new();
    for commit in commits {
        let (classes, counters) = coverage.entry(commit.store.clone()).or_default();
        classes.extend(commit.classes.iter().cloned());
        counters.insert(commit.retired);
    }
    coverage
}

#[test]
fn twin_metadata_allows_interleaving_but_refuses_missing_classes() {
    let pilot = (0..2)
        .map(|index| Commit {
            ordinal: index,
            store: "runtime".into(),
            store_index: index,
            store_ordinal: index,
            label: "commit".into(),
            tables: Vec::new(),
            indexed: true,
            classes: vec![format!("change:{index}")],
            retired: index,
        })
        .collect::<Vec<_>>();
    let mut reordered = pilot.clone();
    reordered.reverse();
    assert_eq!(metadata_coverage(&pilot), metadata_coverage(&reordered));
    reordered[0].classes.clear();
    assert_ne!(metadata_coverage(&pilot), metadata_coverage(&reordered));
}

/// Cover every SQL publication, changed operation/state and retained anchor
/// class seen in the twin pilot. Retirement witnesses also cover anchors at
/// the actual counter increase, with neighbouring runtime publications cut.
pub(super) fn representative_cuts(pilot: &[Commit]) -> Vec<usize> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut previous = BTreeMap::new();
    let mut retirements = BTreeSet::new();
    let facts = pilot
        .iter()
        .enumerate()
        .map(|(index, commit)| {
            let mut classes = commit.classes.iter().cloned().collect::<BTreeSet<_>>();
            classes.insert(format!(
                "sql:{}:{}:{:?}",
                commit.store, commit.label, commit.tables
            ));
            if commit.store == "runtime" && commit.label == "commit" {
                let prior = previous
                    .insert(commit.store.as_str(), commit.retired)
                    .unwrap_or(0);
                if commit.retired > prior {
                    retirements.insert(index);
                    classes.insert("retirement".into());
                    classes.insert(
                        if prior == 0 {
                            "counter:starts"
                        } else {
                            "counter:increases"
                        }
                        .into(),
                    );
                    classes.extend(
                        commit
                            .classes
                            .iter()
                            .filter(|class| class.starts_with("anchor:"))
                            .map(|class| format!("retirement:{class}")),
                    );
                }
            }
            classes
        })
        .collect::<Vec<_>>();
    assert!(
        !retirements.is_empty(),
        "retirement pilot did not retire evidence"
    );
    let mut uncovered = facts.iter().flatten().cloned().collect::<BTreeSet<_>>();
    let mut selected = BTreeSet::new();
    while !uncovered.is_empty() {
        let index = facts
            .iter()
            .enumerate()
            .max_by_key(|(index, classes)| {
                (
                    classes.intersection(&uncovered).count(),
                    std::cmp::Reverse(*index),
                )
            })
            .expect("nonempty pilot")
            .0;
        assert!(selected.insert(index), "uncovered publication class");
        uncovered.retain(|class| !facts[index].contains(class));
    }
    for index in selected.clone().intersection(&retirements) {
        if let Some(before) = (0..*index)
            .rev()
            .find(|other| pilot[*other].store == "runtime")
        {
            selected.insert(before);
        }
        if let Some(after) =
            (*index + 1..pilot.len()).find(|other| pilot[*other].store == "runtime")
        {
            selected.insert(after);
        }
    }
    for store in pilot
        .iter()
        .map(|commit| commit.store.as_str())
        .collect::<BTreeSet<_>>()
    {
        selected.insert(
            pilot
                .iter()
                .position(|commit| commit.store == store)
                .expect("store start"),
        );
        selected.insert(
            pilot
                .iter()
                .rposition(|commit| commit.store == store)
                .expect("store end"),
        );
    }
    assert!(
        selected.len() <= 40,
        "representative retirement cuts exceeded the bounded budget: {}",
        selected.len()
    );
    selected.into_iter().collect()
}

#[test]
fn representative_retirement_cuts_cover_classes_and_adjacent_publications() {
    let pilot = (0..100)
        .map(|index| Commit {
            ordinal: index,
            store: "runtime".into(),
            store_index: index,
            store_ordinal: index,
            label: "commit".into(),
            tables: vec!["heads".into()],
            indexed: true,
            classes: vec![
                format!("change:{}", index % 5),
                format!("anchor:{}", index % 3),
            ]
            .into_iter()
            .chain((index >= 75).then(|| "anchor:late".into()))
            .collect(),
            retired: u64::from(index >= 50) + u64::from(index >= 80),
        })
        .collect::<Vec<_>>();
    let selected = representative_cuts(&pilot);
    let classes = selected
        .iter()
        .flat_map(|index| pilot[*index].classes.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        classes,
        pilot
            .iter()
            .flat_map(|commit| commit.classes.clone())
            .collect()
    );
    for index in [49, 50, 51, 79, 80, 81] {
        assert!(selected.contains(&index));
    }
    assert!(selected.len() < 16);
    assert_eq!(selected, representative_cuts(&pilot));
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
        classes: Vec::new(),
        retired: 0,
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
        classes: Vec::new(),
        retired: 0,
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
        classes: Vec::new(),
        retired: 0,
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
        classes: Vec::new(),
        retired: 0,
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
