//! Shared Responses-family ModelEvent projector core (xAI / OpenRouter / OpenAI host).
//!
//! ## Ownership
//!
//! | Concern | This module | Dialect plugins |
//! | --- | --- | --- |
//! | Limits / id sources / block state / finish policies | yes | type aliases under dialect names |
//! | Append / complete / location / canonical finish | yes | map [`FamilyProjectionError`] |
//! | SoftSeedEmptyOnly + DeferToFinish | yes ([`ProjectorPolicy::openai_platform`]) | OpenAI host selects policy |
//! | Event-kind dispatch | — | dialect stream event enums |
//! | Tool admission / attempt binding | — | dialect |
//! | Replay envelopes (kind strings, bound payload) | helpers + target hooks | dialect kinds + binding structs |
//! | OpenAI residual (citations, custom tools, incomplete) | — | `plakous-openai` hooks on this core |

mod core;
mod error;

pub use core::{
    CanonicalIdSources, ProjectedBlock, ProjectedItem, ProjectedItemKind, ProjectorLimits,
    ProjectorPolicy, ReasoningEmitMode, ResponseProjectorCore, ToolArgsFinishPolicy,
};
pub use error::FamilyProjectionError;
