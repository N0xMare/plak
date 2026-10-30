//! Terminal-only status glyphs.

/// Painted for a label no projection produces. A label reaching this glyph is
/// a silent degradation, so `PROJECTED_STATUS_LABELS` pins the complete set
/// and the host asserts its own enums against it.
pub const UNKNOWN_STATUS_GLYPH: char = '\u{b7}';

/// Status glyph per spec 02 §5.7. Node boxes never animate, so `◐` is static.
///
/// Every arm is spelled out: the wildcard exists only to keep the function
/// total over `&str`, and reaching it is the bug
/// `every_projected_status_label_has_its_own_glyph` catches. `sealing`,
/// `adopting`, `adopted`, and `blocked` are the lifecycle observations, so
/// they carry glyphs of their own rather than sharing the fallback dot.
#[must_use]
pub fn status_glyph(status: &str) -> char {
    match status {
        "pending" => '\u{25cb}',
        "idle" => '\u{25cc}',
        "running" | "submitted" => '\u{25d0}',
        "paused" => '\u{25d1}',
        // Sealing is work in flight on the node itself.
        "sealing" => '\u{25cd}',
        // Adopting is work in flight on the candidate's workspace row.
        "adopting" => '\u{25ce}',
        "discarding" => '\u{25a1}',
        // A landed candidate: the filled cousin of `adopting`.
        "adopted" => '\u{25c9}',
        "completed" | "active" => '\u{25cf}',
        "completed_without_submission" => '\u{25d3}',
        "interrupted" | "closed" | "cancelled" | "clean_removed" | "discarded" => '\u{25a0}',
        // A quota refusal is not a crash; it is a barred road with a reset.
        "blocked" => '\u{2298}',
        "failed" | "recovery_required" => '\u{d7}',
        _ => UNKNOWN_STATUS_GLYPH,
    }
}
