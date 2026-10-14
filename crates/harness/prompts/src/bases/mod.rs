//! The per-family Direct base instruction each recommended profile carries.
//!
//! Two variants ship. `parity` is the default: the Codex family carries the
//! kept sections of Codex's own base instructions, copied verbatim at
//! `vendor/codex-prompt`, and the families whose vendors publish no prompt
//! carry a Plakous-authored block of the same behaviour and roughly the same
//! size. `legacy` is the ~400-character base that shipped before, kept so the
//! eval plane can measure one against the other; the switch is re-exported here.
//!
//! Every base accessor takes its variant as an argument. Reading
//! `PLAK_PROMPT_VARIANT` is [`PromptVariant::from_env`], and calling it is the
//! product's job: no library-reachable path here decides model-visible text
//! from the process environment.

#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

mod claude;
mod codex;
pub mod codex_vendor;
mod grok;
pub mod legacy;
mod pi;
pub mod plak_authored;
mod variant;

pub use claude::claude_direct_base_instructions_for;
pub use codex::{
    codex_code_mode_base_instructions_template_for, codex_direct_base_instructions_for,
    render_codex_code_mode_base_instructions_for,
};
pub use grok::grok_direct_base_instructions_for;
pub use pi::{PI_PREAMBLE, pi_base_instructions};
pub use variant::{
    PLAK_PROMPT_VARIANT_ENV, PLAK_PROMPT_VARIANT_LEGACY, PLAK_PROMPT_VARIANT_PARITY, PromptVariant,
};
