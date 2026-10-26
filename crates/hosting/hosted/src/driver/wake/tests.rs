use super::*;
use proptest::prelude::*;

fn sources(arrivals: &[&str]) -> WakeSources {
    WakeSources {
        running: vec!["p1".into()],
        arrivals: arrivals.iter().map(|id| (*id).into()).collect(),
        children: Vec::new(),
        claimed: Vec::new(),
    }
}

#[test]
fn root_quiet_window_resets_but_child_coalesce_does_not() {
    let now = Instant::now();
    for policy in [WakePolicy::Root, WakePolicy::Child] {
        let mut wake = WakeCoalescer::new(policy);
        assert_eq!(
            wake.observe(&sources(&["m1"]), now),
            WakeDecision::Wait {
                until: now + policy.window()
            }
        );
        let later = now + Duration::from_millis(500);
        let until = match policy {
            WakePolicy::Root => later + policy.window(),
            WakePolicy::Child => now + policy.window(),
        };
        assert_eq!(
            wake.observe(&sources(&["m1", "m2"]), later),
            WakeDecision::Wait { until }
        );
        assert_eq!(
            wake.observe(&sources(&["m1", "m2"]), until),
            WakeDecision::Fire(sources(&["m1", "m2"]).arrivals)
        );
    }
}

#[test]
fn only_children_wake_when_live_work_vanishes_and_claimed_mail_delays_them() {
    let now = Instant::now();
    let mut empty = sources(&[]);
    empty.running.clear();
    empty.claimed.push("in-flight".into());
    let mut child = WakeCoalescer::new(WakePolicy::Child);
    assert_eq!(child.observe(&empty, now), WakeDecision::Idle);
    empty.claimed.clear();
    assert_eq!(
        child.observe(&empty, now),
        WakeDecision::Wait {
            until: now + Duration::from_secs(1)
        }
    );
    assert_eq!(
        WakeCoalescer::new(WakePolicy::Root).observe(&empty, now),
        WakeDecision::Idle
    );
}

proptest! {
    #[test]
    fn carried_arrivals_never_refire_while_work_is_live(ids in prop::collection::btree_set("[a-z0-9]{1,8}", 1..30)) {
        let now = Instant::now();
        let sources = WakeSources { running: vec!["p1".into()], arrivals: ids.clone(), children: Vec::new(), claimed: Vec::new() };
        let mut wake = WakeCoalescer::new(WakePolicy::Child);
        wake.carried(ids);
        prop_assert_eq!(wake.observe(&sources, now), WakeDecision::Idle);
        prop_assert_eq!(wake.observe(&sources, now + Duration::from_secs(60)), WakeDecision::Idle);
    }
}
