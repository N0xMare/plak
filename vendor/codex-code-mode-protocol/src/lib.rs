//! Reviewed vendor snapshot of the pinned Codex Code Mode presentation grammar.
//!
//! Only the closure Plakous actually calls is vendored: the model-visible
//! `exec`/`wait` descriptions, the `exec` input grammar, the JSON-Schema to
//! TypeScript renderer they use, and the tool identity those descriptions name.
//! The upstream gRPC host, session, and runtime modules are deliberately absent,
//! which is what removes `prost`, `tonic`, and the vendored `protoc` build step
//! from every consumer's dependency graph.
//!
//! `PROVENANCE.md` records the upstream repository, revision, version, and the
//! exact files and line ranges copied. Do not edit `description.rs`,
//! `json_schema_types.rs`, or `tool_name.rs`: they are verbatim upstream text,
//! and the rendered descriptions are digest-pinned by Plakous.
//!
//! Upstream splits these files across two crates, so `description.rs` opens with
//! `use codex_protocol::ToolName;`. The self-alias below reproduces that name
//! against this crate's own re-export, which is what keeps the copied files
//! byte-identical and a re-vendor a plain `diff`.

#![allow(
    clippy::all,
    clippy::pedantic,
    missing_docs,
    reason = "verbatim upstream source, kept byte-identical for provenance"
)]

extern crate self as codex_protocol;

mod description;
mod json_schema_types;
mod tool_name;

pub use description::CODE_MODE_PRAGMA_PREFIX;
pub use description::CodeModeToolKind;
pub use description::EnabledToolMetadata;
pub use description::ImageDetailVisibility;
pub use description::ParsedExecSource;
pub use description::ToolDefinition;
pub use description::ToolNamespaceDescription;
pub use description::augment_tool_definition;
pub use description::build_exec_tool_description;
pub use description::build_wait_tool_description;
pub use description::enabled_tool_metadata;
pub use description::is_code_mode_nested_tool;
pub use description::normalize_code_mode_identifier;
pub use description::parse_exec_source;
pub use description::render_code_mode_sample;
pub use json_schema_types::render_json_schema_to_typescript;
pub use tool_name::DEFAULT_FUNCTION_NAMESPACE;
pub use tool_name::ToolName;

/// Model-visible name of the Code Mode evaluation tool.
pub const PUBLIC_TOOL_NAME: &str = "exec";
/// Model-visible name of the Code Mode resumption tool.
pub const WAIT_TOOL_NAME: &str = "wait";
