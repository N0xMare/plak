//! Immutable view structs an observation carries: the presentation read
//! model, catalog and credential rows, navigator rows, resume rows, and
//! template readiness. Presentation-only and product-neutral; nothing here
//! is transcript, checkpoint, or recovery authority.

pub mod presentation;
pub mod transcript;

mod agent_transcript;
mod call_detail;
mod catalog;
mod navigator;
mod processes;
mod resume;
mod templates;
mod tree;

pub use agent_transcript::AgentTranscript;
pub use call_detail::{
    CallDetail, CallDetailOutcome, CallDetailSource, MAX_SOURCE_LINKS, SourceLink,
    parse_source_links,
};
pub use catalog::{
    AuthKind, ExtraState, ModelEntry, ProviderDef, ProviderSection, ProviderState, ReadyCatalog,
};
pub use navigator::{
    AttemptCompaction, AttemptUsage, BlockedObservation, PROJECTED_STATUS_LABELS,
    UNKNOWN_STATUS_GLYPH, status_glyph,
};
pub use presentation::{
    ActivityKind, ContextUsage, PresentationSnapshot, SessionInfoView, SubscriptionUsageView,
    UsageLimitsView,
};
pub use processes::{ProcessStatus, ProcessView};
pub use resume::ResumeSessionEntry;
pub use templates::TemplateRow;
pub use tree::{ChildProgress, NodeView, TreeView};
