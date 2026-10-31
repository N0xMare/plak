//! Structured scrollback blocks for the product TUI.
//!
//! A facade over its children: the block model and its row text (`model`),
//! fold plans and the plain layout (`layout`), tool and approval rows
//! (`tool_rows`), cell-width wrapping (`wrap`) and the styled transcript
//! renderer with its cache (`render`).

mod layout;
mod model;
mod plan;
mod render;
mod tool_rows;
mod wrap;

#[cfg(test)]
mod tests;

pub use layout::*;
pub use model::*;
pub use tool_rows::*;
pub use wrap::*;

#[cfg(test)]
pub(super) use render::TRANSCRIPT_RENDER_CACHE_BYTES;
#[cfg(test)]
pub(in crate::tui) use render::code_line_rows_for_test;
pub(super) use render::{LeadRow, TranscriptChange, TranscriptRenderCache, lead_rows};
