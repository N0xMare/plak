//! How a model reaches its tools when it does not call them directly.
//!
//! A *composition* replaces the presented tool menu with one or two outer
//! tools that take code, and runs the pack tools as calls inside that code.
//! Three exist: the product-owned JavaScript cell (`exec_js`/`wait_js`), the
//! Python cell (`exec_py`), and Codex's own Code Mode (`exec`/`wait`).
//!
//! Each one is a *presentation of the coding packs*: it renders their nested
//! catalog into the outer tool's description and compiles a nested projection
//! that lowers onto the same canonical capabilities the Direct menu uses. That
//! is why this crate sees the packs, and why the instruction text does not
//! (`harness-pack-independent` in the root manifest).
//!
//! What is *not* here is the durable identity of the outer markers: their
//! names, ids, revisions, schemas and definitions stay in
//! `plakous_tools::composition_markers`, because a session descriptor is
//! validated against them when it reopens.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact tool identifiers and composition ids"
)]

mod candidate;
mod codex_code_mode;
mod coding_menu;
mod javascript;
mod python;
mod sidecar;
mod text;

pub use candidate::*;
pub use codex_code_mode::*;
pub use coding_menu::*;
pub use javascript::*;
pub use python::*;
pub use sidecar::SidecarLocator;
