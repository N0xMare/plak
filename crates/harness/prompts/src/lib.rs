//! Every sentence Plakous shows a model that is not a tool's own description.
//!
//! Tool descriptions live with their tool (spec 02 §6.3): the pack that owns a
//! capability owns the text that explains it. What is left is *instruction*
//! text — who the agent is, how it works, how an assignment starts — and the
//! one assembler that writes it in spec 02 §6.1 order.
//!
//! - [`bases`] — the per-family Direct base instruction each recommended
//!   profile carries, and the Code Mode template.
//! - [`profiles`] — the recommended harness profiles that name those bases.
//! - [`identity`] — the identity tail, the always-on working agreements, and
//!   the agreement paragraphs a surface earns.
//! - [`ptc`] — the harness-PTC base instructions and the OpenRouter Direct base.
//! - [`plan`] — [`plan::PromptPlan`], the one assembly and base-selection site.
//! - [`vocabulary`] — the three facts the text names, and nothing else.
//! - [`instructions`] — bounded `AGENTS.md` discovery.
//!
//! The pin tables live with their text; [`pins`] is the only thing that hashes.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact tool identifiers, account names, and composition ids"
)]

pub mod bases;
pub mod content_filter;
pub mod identity;
pub mod instructions;
pub mod pins;
pub mod plan;
pub mod profiles;
pub mod ptc;
pub mod vocabulary;

#[cfg(test)]
mod surface_fixture;
