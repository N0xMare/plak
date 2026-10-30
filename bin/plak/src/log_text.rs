//! Product-only command grammar in existing session-log detail fields.

use plakous_hosted::session_log::{JsonVal, SessionEvent, SessionEventPresentation};

pub(crate) fn render(event: &mut SessionEvent) {
    if event.presentation == Some(SessionEventPresentation::CompactCommittedOverflow) {
        for (name, value) in &mut event.fields {
            if name == "detail" {
                *value = JsonVal::Str("compact workflow committed; retained projection still overflows; retry is a new paid operation or /clear".into());
            }
        }
    }
}
