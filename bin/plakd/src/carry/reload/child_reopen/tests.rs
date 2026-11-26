use super::*;

const SESSION: &str = "10000000-0000-4000-8000-000000000001";
const BRANCH: &str = "10000000-0000-4000-8000-000000000002";
const CHECKPOINT: &str = "10000000-0000-4000-8000-000000000003";

fn with_fault(head: Option<String>, fault: Fault) -> ChildReopenInspection {
    ChildReopenInspection {
        faults: vec![(head, fault)],
        ..Default::default()
    }
}

#[test]
fn without_a_source_baseline_unreadable_children_refuse_and_missing_artifacts_do_not() {
    let head = Some(format!("{SESSION}/{BRANCH}/{CHECKPOINT}"));
    let unreadable = with_fault(
        head.clone(),
        Fault::Unreadable {
            detail: "unreadable".into(),
        },
    );
    assert_eq!(unreadable.qualify().regressions.affected_children, 1);
    let contract = with_fault(
        head.clone(),
        Fault::EvaluatorContract {
            detail: "changed contract".into(),
        },
    );
    assert_eq!(contract.qualify().regressions.affected_children, 1);
    let missing = with_fault(head, Fault::ArtifactMissing);
    let report = missing.qualify();
    assert!(report.regressions.is_empty());
    assert_eq!(
        report.pre_existing.by_kind[&FaultKind::EvaluatorArtifactMissing],
        1
    );
    let headless = with_fault(None, Fault::ArtifactMissing);
    assert_eq!(headless.qualify().regressions.affected_children, 1);
}
