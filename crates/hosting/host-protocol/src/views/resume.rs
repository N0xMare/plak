//! Cwd-scoped resume rows. The host owns discovery and supplies only sessions
//! already filtered to the current workspace, newest first.

/// One host-authoritative session row eligible for exact resume.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResumeSessionEntry {
    /// Exact durable product session id.
    pub id: String,
    /// Persisted account label, when bound.
    pub account: Option<String>,
    /// Persisted exact model id, when bound.
    pub model: Option<String>,
    /// Persisted exact effort label, when selected.
    pub effort: Option<String>,
    /// Persisted exact optional model control (`priority`), when selected.
    pub extra: Option<String>,
    /// Host-projected session title, when one was recorded.
    pub title: Option<String>,
    /// The session's first user prompt, already bounded by the host.
    pub first_prompt: Option<String>,
    /// Session creation time, unix seconds.
    pub created_at_unix: u64,
    /// Last-updated time, unix seconds. This is the age a row shows and the
    /// order the host sorted by.
    pub updated_at_unix: u64,
    /// The current host can activate this exact binding without a terminal relaunch.
    pub in_place: bool,
}
