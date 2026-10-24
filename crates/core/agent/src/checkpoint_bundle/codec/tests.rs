use std::collections::BTreeMap;

use super::{materialize_history, materialize_operation, materialize_working};
use crate::checkpoint_bundle::tests::{
    TestResult, component, every_state_operation_records, fixture,
};
use crate::checkpoint_bundle::{
    BundleBuilder, CheckpointBundleError, ObjectIndex, RuntimeCheckpointCodec, RuntimeObjectKind,
    SEGMENT_DECODES,
};
use crate::manifest::{OperationRecordWire, RuntimeComponentKind, RuntimeManifestError};
use plakous_model::WorkingProjection;

#[test]
fn an_operation_reference_to_non_operation_evidence_fails_closed() -> TestResult {
    let records = every_state_operation_records()?;
    let mut builder = BundleBuilder {
        objects: BTreeMap::new(),
        visited: 0,
    };
    let foreign = builder.component(&component(
        RuntimeComponentKind::Command,
        1,
        b"not-operation-evidence",
    )?)?;
    let mut request = true;
    let mut children = Vec::new();
    let wire = OperationRecordWire::new(&records[1], |component| {
        let address = if std::mem::take(&mut request) {
            foreign
        } else {
            builder.component(component)?
        };
        children.push(address);
        Ok::<_, CheckpointBundleError>(address.digest())
    })?;
    let address = builder.insert(RuntimeObjectKind::OperationRecord, wire.encode()?, children)?;
    let index = ObjectIndex {
        decoded: BTreeMap::new(),
        objects: builder
            .objects
            .iter()
            .map(|(address, object)| (*address, object))
            .collect(),
    };
    let object = index.object(address, RuntimeObjectKind::OperationRecord)?;
    assert_eq!(
        materialize_operation(&index, object),
        Err(CheckpointBundleError::Manifest(
            RuntimeManifestError::ComponentKind
        ))
    );
    Ok(())
}

#[test]
fn history_and_working_materializers_share_the_decoded_segment() -> TestResult {
    let checkpoint = fixture(1, 9200, false)?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let segment = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .next()
        .ok_or("segment")?
        .clone();
    let working = WorkingProjection::new(vec![segment], WorkingProjection::default().limits())?;
    let mut builder = BundleBuilder {
        objects: BTreeMap::new(),
        visited: 0,
    };
    let working_address = builder.working(&working)?;
    SEGMENT_DECODES.set(0);
    let mut index = ObjectIndex::validate(&bundle)?;
    let decoded = SEGMENT_DECODES.get();
    index.objects.insert(
        working_address,
        builder
            .objects
            .get(&working_address)
            .ok_or("working object")?,
    );
    let history = materialize_history(
        &index,
        1,
        bundle.reuse.as_ref().ok_or("reuse")?.history_head,
    )?;
    let working = materialize_working(&index, working_address)?;
    assert!(
        history
            .segments()
            .iter()
            .next()
            .ok_or("history segment")?
            .shares_storage_with(&working.segments()[0])
    );
    assert_eq!(SEGMENT_DECODES.get(), decoded);
    Ok(())
}
