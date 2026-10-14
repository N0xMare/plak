use super::{
    abandoned_compaction_evidence, abandoned_effect_evidence, abandoned_operation_evidence,
    abandoned_process_evidence,
};
use crate::runtime::operation_record::ProviderOutcome;

/// Every abandonment evidence recovery writes is in the closure-tag
/// inventory, so a round provider record carrying it reads as closed,
/// never as corrupt.
#[test]
fn recovery_abandonment_evidence_is_an_enumerated_closure_tag()
-> Result<(), Box<dyn std::error::Error>> {
    for evidence in [
        abandoned_operation_evidence()?,
        abandoned_compaction_evidence()?,
        abandoned_effect_evidence()?,
        abandoned_process_evidence()?,
    ] {
        assert_eq!(ProviderOutcome::decode(&evidence)?, None);
    }
    Ok(())
}
