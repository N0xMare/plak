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
fn only_an_explicit_legacy_audit_fault_on_the_same_branch_excuses_unreadability() {
    let unreadable = || Fault::Unreadable {
        detail: "unreadable".into(),
    };
    let destination = with_fault(
        Some(format!("{SESSION}/{BRANCH}/{CHECKPOINT}")),
        unreadable(),
    );
    let wrong_branch = BTreeSet::from([branch_key(SESSION, CHECKPOINT)]);
    let verified = BTreeSet::from([branch_key(SESSION, BRANCH)]);
    for audit in [None, Some(&wrong_branch)] {
        let report = destination.qualify(ChildBaseline::Legacy(audit));
        assert_eq!(report.regressions.affected_children, 1);
        assert!(report.pre_existing.is_empty());
    }
    let report = destination.qualify(ChildBaseline::Legacy(Some(&verified)));
    assert!(report.regressions.is_empty());
    assert_eq!(
        report.pre_existing.by_kind[&FaultKind::BindingUnreadable],
        1
    );

    let headless = with_fault(None, unreadable());
    assert_eq!(
        headless
            .qualify(ChildBaseline::Legacy(Some(&verified)))
            .regressions
            .affected_children,
        1
    );
    assert_eq!(
        headless
            .qualify(ChildBaseline::Exact(&destination))
            .regressions
            .affected_children,
        1
    );
}

#[test]
fn a_missing_evaluator_in_the_source_does_not_mask_a_contract_regression() {
    let head = Some(format!("{SESSION}/{BRANCH}/{CHECKPOINT}"));
    let source = with_fault(head.clone(), Fault::ArtifactMissing);
    let destination = with_fault(
        head,
        Fault::EvaluatorContract {
            detail: "changed contract".into(),
        },
    );
    let report = destination.qualify(ChildBaseline::Exact(&source));
    assert_eq!(report.regressions.affected_children, 1);
    assert!(
        destination
            .qualify(ChildBaseline::Exact(&ChildReopenInspection::default()))
            .unproven
    );
}
