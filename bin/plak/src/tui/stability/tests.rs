use super::*;

fn frame(rows: &[&str]) -> Vec<String> {
    rows.iter().map(|row| (*row).to_owned()).collect()
}

#[test]
fn a_still_frame_scores_nothing() {
    let mut metric = AnchorStability::new();
    let rows = frame(&["alpha", "beta", "gamma"]);
    metric.observe(&rows, ViewportMotion::Pinned);
    metric.observe(&rows, ViewportMotion::Pinned);
    let report = metric.report();
    assert_eq!(report.repositions, 0);
    assert_eq!(report.mass_reflows, 0);
    assert_eq!(report.transitions, 1);
    assert_eq!(
        report.violations(&StabilityBudget::perfect()),
        [] as [String; 0]
    );
}

#[test]
fn styling_and_trailing_pad_are_not_content() {
    let mut metric = AnchorStability::new();
    metric.observe(&frame(&["alpha", "beta"]), ViewportMotion::Pinned);
    metric.observe(&frame(&["alpha    ", "beta  "]), ViewportMotion::Pinned);
    assert_eq!(metric.report().repositions, 0);
}

#[test]
fn a_pinned_frame_that_moves_content_is_a_reposition() {
    let mut metric = AnchorStability::new();
    metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
    metric.observe(
        &frame(&["inserted", "alpha", "beta", "gamma"]),
        ViewportMotion::Pinned,
    );
    let report = metric.report();
    assert_eq!(report.repositions, 1);
    assert_eq!(report.max_anchor_shift, 1);
    assert_eq!(report.big_pops, 0);
    assert_eq!(report.repositions_per_100_frames(), 100.0);
}

#[test]
fn the_same_move_while_following_the_tail_is_not_a_defect() {
    let mut metric = AnchorStability::new();
    metric.observe(
        &frame(&["alpha", "beta", "gamma"]),
        ViewportMotion::FollowTail,
    );
    metric.observe(
        &frame(&["beta", "gamma", "delta"]),
        ViewportMotion::FollowTail,
    );
    let report = metric.report();
    assert_eq!(report.repositions, 0);
    assert_eq!(report.pinned_transitions, 0);
    assert_eq!(report.repositions_per_100_frames(), 0.0);
}

#[test]
fn losing_the_anchor_is_a_big_pop() {
    let mut metric = AnchorStability::for_viewport(9);
    metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
    metric.observe(&frame(&["one", "two", "three"]), ViewportMotion::Pinned);
    let report = metric.report();
    assert_eq!(report.repositions, 1);
    assert_eq!(report.big_pops, 1);
    assert_eq!(report.mass_reflows, 1);
}

#[test]
fn a_row_that_vanishes_for_one_frame_is_a_blink() {
    let mut metric = AnchorStability::new();
    metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
    metric.observe(&frame(&["alpha", "", "gamma"]), ViewportMotion::Pinned);
    metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
    assert_eq!(metric.report().blinks, 1);
}

#[test]
fn a_held_viewport_over_three_frames_stays_within_the_perfect_budget() {
    let mut metric = AnchorStability::new();
    for _ in 0..3 {
        metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
    }
    let report = metric.report();
    assert_eq!(
        report.violations(&StabilityBudget::perfect()),
        [] as [String; 0]
    );
    assert_eq!(report.frames, 3);
    assert_eq!(report.pinned_transitions, 2);
}

#[test]
fn a_violating_report_names_every_broken_line() {
    let mut metric = AnchorStability::for_viewport(9);
    metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
    metric.observe(&frame(&["one", "two", "three"]), ViewportMotion::Pinned);
    let violations = metric.report().violations(&StabilityBudget::strict());
    assert_eq!(violations.len(), 3, "{violations:?}");
}
