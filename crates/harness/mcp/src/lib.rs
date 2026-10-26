//! Model Context Protocol client for Plakous.
//!
//! A process-wide [`McpPool`] starts each configured server lazily (a stdio
//! server through the execution environment's service-process seam, a
//! streamable HTTP server ([`McpHttpEndpoint`]) over `plakous-http`), shares it among
//! every session bound to the same definition and workspace root, and stops
//! it after an idle period. At bind a [`McpLease`] freezes the template's
//! selection of the server's tools ([`FrozenMcpListing`]): each input schema
//! is lowered onto the portable schema ([`lower_input_schema`]) and each name
//! is exposed as `mcp__<server>__<tool>` ([`exposed_tool_name`]).
//! [`McpToolPack`] registers the frozen tools as serial, mutating tools owned
//! by their server: a call that may have acted settles as outcome-unknown and
//! is never replayed. A lease also lists and reads the server's resources
//! ([`McpResourceContents`]) and lists and expands its prompts
//! ([`McpPromptExpansion`]), which only a user invokes.
//!
//! Configuration parsing, secret storage and operator views belong to the
//! embedding product.

#![forbid(unsafe_code)]

#[cfg(feature = "client")]
#[path = "client_surface.rs"]
mod surface;
#[cfg(not(feature = "client"))]
#[path = "vocabulary_surface.rs"]
mod surface;

pub use surface::*;
