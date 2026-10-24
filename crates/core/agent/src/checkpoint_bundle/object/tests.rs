use super::component_address;
use crate::checkpoint_bundle::tests::{
    EVERY_STATE_EVIDENCE, TestResult, component, contains, every_state_checkpoint, replace_supplied,
};
use crate::checkpoint_bundle::{
    CheckpointBundleError, CheckpointObjectBundle, RuntimeCheckpointCodec, RuntimeObject,
    RuntimeObjectKind,
};
use crate::manifest::{RuntimeComponentKind, encode_bundle_component};

#[test]
fn operation_records_reference_every_state_component_and_reopen_exactly() -> TestResult {
    let checkpoint = every_state_checkpoint()?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let records = bundle
        .objects()
        .iter()
        .filter(|object| object.address().kind() == RuntimeObjectKind::OperationRecord)
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 4);
    for record in &records {
        assert!(
            record
                .children()
                .iter()
                .all(|child| child.kind() == RuntimeObjectKind::ReconstructiveComponent)
        );
        for evidence in EVERY_STATE_EVIDENCE {
            assert!(
                !contains(record.canonical_bytes(), evidence),
                "record inlines {}",
                String::from_utf8_lossy(evidence)
            );
        }
    }
    for evidence in EVERY_STATE_EVIDENCE {
        let address = component_address(&component(RuntimeComponentKind::Operation, 1, evidence)?)?;
        assert!(
            records
                .iter()
                .any(|record| record.children().contains(&address)),
            "no record references {}",
            String::from_utf8_lossy(evidence)
        );
    }
    assert_eq!(RuntimeCheckpointCodec::materialize(&bundle)?, checkpoint);
    let reopened = CheckpointObjectBundle::from_store(
        bundle.checkpoint_id(),
        bundle.checkpoint_integrity(),
        bundle.root(),
        bundle.objects().to_vec(),
    )?;
    assert_eq!(reopened.validated_checkpoint()?.as_ref(), &checkpoint);
    Ok(())
}

#[test]
fn a_missing_or_tampered_referenced_component_fails_closed() -> TestResult {
    let checkpoint = every_state_checkpoint()?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let claim = component_address(&component(
        RuntimeComponentKind::Operation,
        1,
        b"effect-claim",
    )?)?;
    let position = bundle
        .objects()
        .iter()
        .position(|object| object.address() == claim)
        .ok_or("claim component object is absent")?;
    let reopen = |objects: Vec<RuntimeObject>| {
        CheckpointObjectBundle::from_store(
            bundle.checkpoint_id(),
            bundle.checkpoint_integrity(),
            bundle.root(),
            objects,
        )
        .and_then(|reopened| reopened.validated_checkpoint().map(|_| ()))
    };

    let mut objects = bundle.objects().to_vec();
    objects.remove(position);
    assert_eq!(
        reopen(objects.clone()),
        Err(CheckpointBundleError::MissingObject)
    );
    let mut missing = bundle.clone();
    replace_supplied(&mut missing, objects);
    assert_eq!(
        RuntimeCheckpointCodec::materialize(&missing),
        Err(CheckpointBundleError::MissingObject)
    );

    let forged = encode_bundle_component(&component(
        RuntimeComponentKind::Operation,
        1,
        b"forged-claim",
    )?)?;
    let mut objects = bundle.objects().to_vec();
    objects[position].canonical_bytes = forged.clone().into();
    assert_eq!(
        RuntimeObject::from_store(claim, forged, Vec::new()),
        Err(CheckpointBundleError::InvalidAddress)
    );
    let mut tampered = bundle.clone();
    replace_supplied(&mut tampered, objects);
    assert_eq!(
        RuntimeCheckpointCodec::materialize(&tampered),
        Err(CheckpointBundleError::InvalidAddress)
    );

    let record = bundle
        .objects()
        .iter()
        .find(|object| object.children().contains(&claim))
        .ok_or("no record references the claim")?;
    let mut redirected = record.children().to_vec();
    for child in &mut redirected {
        if *child == claim {
            *child = component_address(&component(
                RuntimeComponentKind::Operation,
                1,
                b"effect-unknown",
            )?)?;
        }
    }
    assert_eq!(
        RuntimeObject::from_store(
            record.address(),
            record.canonical_bytes().to_vec(),
            redirected
        ),
        Err(CheckpointBundleError::NonCanonical)
    );
    Ok(())
}
