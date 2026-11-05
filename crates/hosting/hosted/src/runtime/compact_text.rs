//! Operator-facing compaction and context-capacity texts.

pub(super) const COMPACT_NOT_NEEDED_DETAIL: &str =
    "compaction not needed: the selected prefix already fits";

pub(crate) const COMPACT_NO_ELIGIBLE_RANGE_DETAIL: &str =
    "nothing in this conversation can be compacted yet";

pub(crate) const COMPACT_FAILED_DETAIL: &str =
    "compact workflow finished without a compacted checkpoint";

/// The one notice of an admitted compaction a closure abandoned with exact
/// evidence that its summary request was never sent.
pub(crate) const COMPACT_ABANDONED_UNSENT_NOTE: &str = "plak: interrupted compaction was not \
     applied; its summary was not sent and will not be dispatched again; the conversation history \
     is unchanged";

/// The one notice of an admitted compaction a closure abandoned without that
/// evidence.
pub(crate) const COMPACT_ABANDONED_BILLED_NOTE: &str = "plak: interrupted compaction was not \
     applied, may have been billed, and will not be dispatched again; the conversation history is \
     unchanged";

pub(super) const COMPACT_PROVIDER_QUIET_NOTE: &str = "compact didn't finish (the provider went quiet). Context is unchanged. Another explicit compaction may retry";

pub(super) const COMPACT_RECOVERY_REQUIRED_DETAIL: &str =
    "canonical compact requires explicit recovery; prompts remain paused";

pub(super) const COMPACTING_ACTIVITY_LABEL: &str = "compacting context";

pub(super) const COMPACT_COMMITTED_DETAIL: &str = "compact workflow committed";

pub(super) const COMPACT_COMMITTED_STILL_OVERFLOWS_DETAIL: &str = "compact workflow committed; retained projection still overflows; another attempt is a new paid operation, or start a fresh conversation";

pub(crate) const CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE: &str = "tool-grown context reached the admitted ceiling; the failed root turn is terminal and will not be replayed. An explicit whole-group compaction may target this exact checkpoint; it does not guarantee the retained projection will fit";

pub(crate) const CONTEXT_CAPACITY_NATIVE_COMPACT_GUIDANCE: &str = "tool-grown context reached the admitted ceiling; the failed root turn is terminal and will not be replayed. An explicit native compaction may target this exact checkpoint; it does not fold the latest user message and does not guarantee the retained projection will fit";

pub(crate) const CONTEXT_CAPACITY_SEGMENT_LIMIT_GUIDANCE: &str = "the session's history reached its segment limit, which counts history segments rather than tokens; the failed root turn is terminal and will not be replayed. Compact earlier history in this session, compact a fork while leaving this session unchanged, or start a new conversation";

pub(crate) const CONTEXT_CAPACITY_PROTECTED_GUIDANCE: &str = "tool-grown context overflow cannot be compacted; choose a model with a larger admitted ceiling, fork, or start a fresh conversation";

pub(crate) const CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE: &str = "context-capacity recovery is unavailable because its exact source could not be verified; choose a larger model or start a new session";
