use super::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn sid(number: u128) -> TestResult<SessionId> {
    Ok(uuid::Uuid::from_u128(number).try_into()?)
}

#[test]
fn move_groups_shared_forks_move_and_abandon_together_while_an_unrelated_tree_is_independent()
-> TestResult {
    let trees = BTreeMap::from([
        ("fork-a".into(), BTreeSet::from([sid(1)?])),
        ("fork-b".into(), BTreeSet::from([sid(1)?])),
        ("unrelated".into(), BTreeSet::from([sid(2)?])),
    ]);
    let plan = MoveGroups::plan("operation", &trees, &BTreeMap::new())?;
    assert_eq!(plan.groups().len(), 2);
    let shared = plan
        .groups()
        .iter()
        .find(|group| group.trees.len() == 2)
        .ok_or("shared group")?;
    assert_eq!(
        shared.trees(),
        &BTreeSet::from(["fork-a".into(), "fork-b".into()])
    );
    for tree in ["fork-a", "fork-b"] {
        assert_eq!(
            plan.resolve_abandon(tree).err(),
            Some(GroupPlanError::ExplicitGroupRequired(shared.id().into()))
        );
    }
    assert_eq!(plan.resolve_abandon(shared.id())?, shared);
    assert_eq!(
        plan.resolve_abandon("unrelated")?.trees(),
        &BTreeSet::from(["unrelated".into()])
    );
    assert_eq!(
        plan,
        MoveGroups::plan("operation", &trees, &BTreeMap::new())?
    );
    assert_ne!(
        plan,
        MoveGroups::plan("another-operation", &trees, &BTreeMap::new())?
    );
    Ok(())
}

#[test]
fn move_groups_close_retired_branch_dependencies_in_both_directions_without_overlapping_groups()
-> TestResult {
    let trees = BTreeMap::from([
        ("a-current-head".into(), BTreeSet::from([sid(3)?])),
        ("z-retired-member".into(), BTreeSet::from([sid(1)?])),
        ("unrelated".into(), BTreeSet::from([sid(9)?])),
    ]);
    let heads = BTreeMap::from([(sid(1)?, BTreeSet::from([sid(3)?]))]);
    let plan = MoveGroups::plan("operation", &trees, &heads)?;
    assert_eq!(plan.groups().len(), 2);
    let shared = plan
        .groups()
        .iter()
        .find(|group| group.trees.len() == 2)
        .ok_or("dependency group")?;
    assert_eq!(shared.sessions(), &BTreeSet::from([sid(1)?, sid(3)?]));
    assert_eq!(
        plan.groups()
            .iter()
            .map(|group| group.trees.len())
            .sum::<usize>(),
        trees.len()
    );
    let missing = BTreeMap::from([(sid(1)?, BTreeSet::from([sid(17)?]))]);
    assert_eq!(
        MoveGroups::plan("operation", &trees, &missing).err(),
        Some(GroupPlanError::UnmappedHead(sid(17)?))
    );
    Ok(())
}
