use super::*;

fn discovered(line: &Line, index: u32) -> Line {
    let mut row = line.clone();
    row.owner.line = id(&format!("line-{index}"));
    row.jobs[0].session = id(&format!("allocation-{index}"));
    row
}

#[test]
fn retention_observations_cache_only_requested_lines_from_more_than_256_discoveries() {
    let (_root, registry, scope, line, _) = fixture();
    registry.observation_connected("home", "receiver", 1);
    let discoveries = (0..300)
        .map(|index| discovered(&line, index))
        .collect::<Vec<_>>();
    let own = Origin {
        build: id("receiver-build"),
        version: 1,
        instance: id("receiver"),
    };
    let requested = vec!["line-1".into(), "line-299".into()];
    registry
        .remember_requested(&scope, &own, &requested, Some(1), &discoveries)
        .unwrap();
    assert_eq!(lock(&registry.observed).lines.len(), 2);
    assert_eq!(
        registry
            .occupancy(|_, line| requested.iter().any(|id| id == line))
            .values()
            .sum::<usize>(),
        2
    );
    let unrequested = vec!["line-25".into()];
    registry.forget_observations("home", "receiver", &unrequested);
    assert_eq!(lock(&registry.observed).lines.len(), 2);
}

#[test]
fn retention_observations_forgetting_disposed_lines_preserves_independent_groups_and_hosts() {
    let (_root, registry, scope, line, to) = fixture();
    registry
        .remember(&scope, [discovered(&line, 1), discovered(&line, 2)])
        .unwrap();
    let mut other = scope.clone();
    other.host_id = id("other-host");
    registry.remember(&other, [discovered(&line, 1)]).unwrap();
    let before = serde_json::to_vec(&registry.all_entries()).unwrap();
    for _ in 0..2 {
        registry.forget_observations("home", "receiver", &["line-1".into(), "unknown".into()]);
    }
    assert_eq!(lock(&registry.observed).lines.len(), 2);
    assert_eq!(
        registry
            .occupancy(|host, line| host == "home" && line == "line-2")
            .values()
            .sum::<usize>(),
        1
    );
    assert_eq!(
        registry
            .occupancy(|host, _| host == "other-host")
            .values()
            .sum::<usize>(),
        1
    );
    assert_eq!(serde_json::to_vec(&registry.all_entries()).unwrap(), before);
    assert_eq!(registry.entries("home", None)[0].to, to);
}

#[test]
fn retention_observations_reconnect_rejects_late_writes_and_clears_only_the_same_instance() {
    let (_root, registry, scope, line, _) = fixture();
    registry.observation_connected("home", "receiver", 1);
    registry.remember(&scope, [discovered(&line, 1)]).unwrap();
    let mut other = scope.clone();
    other.host_id = id("other-host");
    registry.remember(&other, [discovered(&line, 2)]).unwrap();
    registry.observation_connected("home", "receiver", 2);
    assert!(
        registry
            .remember_at(&scope, Some(1), [discovered(&line, 3)])
            .is_err()
    );
    registry.observation_disconnected("home", "receiver", 1);
    assert_eq!(registry.observation_connection(&scope), Some(2));
    assert_eq!(lock(&registry.observed).lines.len(), 1);
    assert_eq!(
        registry
            .occupancy(|host, _| host == "other-host")
            .values()
            .sum::<usize>(),
        1
    );
    assert_eq!(
        registry.all_entries().len(),
        1,
        "reconnect dropped a durable intent"
    );
}

#[test]
fn retention_observations_two_live_instances_have_independent_line_capacity() {
    let (_root, registry, first, line, _) = fixture();
    let mut second = first.clone();
    second.host_instance = id("second");
    registry.observation_connected("home", "receiver", 1);
    registry
        .remember_at(
            &first,
            Some(1),
            (0..256).map(|index| discovered(&line, index)),
        )
        .unwrap();
    registry.observation_connected("home", "second", 2);
    registry
        .remember_at(
            &second,
            Some(2),
            (256..512).map(|index| discovered(&line, index)),
        )
        .unwrap();
    assert_eq!(lock(&registry.observed).lines.len(), 512);
    assert!(
        registry
            .remember_at(&first, Some(1), [discovered(&line, 512)])
            .is_err()
    );
    registry.forget_observations("home", "receiver", &["line-0".into()]);
    registry
        .remember_at(&first, Some(1), [discovered(&line, 512)])
        .unwrap();
    registry.observation_disconnected("home", "receiver", 1);
    assert_eq!(lock(&registry.observed).lines.len(), 256);
    assert_eq!(registry.observation_connection(&second), Some(2));
}

#[test]
fn retention_observations_terminal_and_positive_loss_prune_ephemeral_rows() {
    let (_root, registry, scope, line, _) = fixture();
    registry.remember(&scope, [discovered(&line, 1)]).unwrap();
    let mut terminal = discovered(&line, 1);
    terminal.jobs[0].terminal = true;
    registry.remember(&scope, [terminal]).unwrap();
    assert!(lock(&registry.observed).lines.is_empty());
    registry.remember(&scope, [discovered(&line, 2)]).unwrap();
    let mut lost = discovered(&line, 2);
    lost.lost = true;
    registry.remember(&scope, [lost]).unwrap();
    assert!(lock(&registry.observed).lines.is_empty());
}
