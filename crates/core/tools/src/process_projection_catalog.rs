//! The provider route tuples the neutral tools layer knows by name.
//!
//! The `CODEX_PROJECTION_*` and `OPENAI_PLATFORM_PROJECTION_*` rows are `pub`:
//! `plakous-compose` compares a Code Mode descriptor's target against them,
//! and a Code Mode presentation is only ever compiled for those two routes.
//!
//! No model id lives here: which literal models a route enrolls is the
//! provider crate's enrollment table, reached through the facade.

// Only the Code Mode tuples are left in this catalog: `codex_code_mode.rs` compares
// a Code Mode descriptor's target against it. Other route tuples belong
// to the composition root: the route -> vocabulary rule table is that
// root's data, not this crate's.
/// The one protocol a Code Mode presentation is compiled for.
pub const CODEX_PROJECTION_PROTOCOL: &str = "openai.codex.responses";
/// The one route a Code Mode presentation is compiled for.
pub const CODEX_PROJECTION_ROUTE: &str = "openai.codex_subscription";
/// The one integration profile a Code Mode presentation is compiled for.
pub const CODEX_PROJECTION_PROFILE: &str = "openai.codex.compat.2026-07-31.r1";
/// The `openai` API protocol a Code Mode presentation is also compiled for.
pub const OPENAI_PLATFORM_PROJECTION_PROTOCOL: &str = "openai.responses";
/// The `openai` API route a Code Mode presentation is also compiled for. Which
/// profiles and models it carries is the route's own enrollment.
pub const OPENAI_PLATFORM_PROJECTION_ROUTE: &str = "openai.platform";
