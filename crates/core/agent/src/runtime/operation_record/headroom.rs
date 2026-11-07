//! Reservations protect settlement from new unjournaled tool work.

// Nested work needs a child edge, effect and spanning process lifetime; it
// refuses decisions. Top-level work can need a decision, effect, process
// lifetime and hosted baton. Four records conservatively cover either path.
// Transitions and delivery acknowledgements replace records rather than append.
pub(in crate::runtime) const TOOL_REQUEST_RECORD_RESERVATION: usize = 4;
const CHARGED_OPERATION_LIMIT: usize = 3_584;
pub(in crate::runtime) const NESTED_HEADROOM_REFUSAL: &str = "nested call not admitted: runtime operation headroom is low; finish this cell and continue in a new call";
pub(in crate::runtime) const BATCH_HEADROOM_REFUSAL: &str = "tool call not admitted: runtime operation headroom is low; finish this batch and continue in a new call";

pub(in crate::runtime) fn has_operation_headroom(
    operations: usize,
    accepted_pending: usize,
) -> bool {
    let limit = crate::test_hooks::ledger_headroom_limit(CHARGED_OPERATION_LIMIT);
    operations < limit
        && operations.saturating_add(
            accepted_pending
                .saturating_add(1)
                .saturating_mul(TOOL_REQUEST_RECORD_RESERVATION),
        ) <= limit
}
