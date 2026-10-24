use super::checkpoint_prefix_mismatch;

#[test]
fn an_empty_prefix_drift_keeps_the_plain_checkpoint_mismatch() {
    use plakous_model::{StableRequestPrefixDrift, StableRequestPrefixField};
    assert_eq!(
        checkpoint_prefix_mismatch(StableRequestPrefixDrift::default()),
        "builder semantics disagree with the durable checkpoint"
    );
    assert_eq!(
        checkpoint_prefix_mismatch(
            StableRequestPrefixDrift::default()
                .with(StableRequestPrefixField::Instructions)
                .with(StableRequestPrefixField::ToolChoice)
        ),
        "builder semantics disagree with the durable checkpoint \
             (stable-prefix fields: instructions,tool-choice)"
    );
}
