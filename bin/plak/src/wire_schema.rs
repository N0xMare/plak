//! `plak wire schema`: the supported JSON Schema (draft 2020-12) of this
//! build's wire v2 frames, both directions, with Plak's extension traffic,
//! and the protocol version and binary they belong to. A client checks its
//! own frames against the exact build it speaks to.

use std::io::Write;

use plakous_host_protocol::wire::{ClientFrame, HostFrame, WIRE_VERSION};
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use schemars::generate::SchemaSettings;

use crate::surface_protocol::PlakExtension;

/// The schema document: `client_frame` and `host_frame` reference their
/// definitions under `$defs`.
#[must_use]
pub(crate) fn document() -> serde_json::Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    let client = generator.subschema_for::<ClientFrame<PlakExtension>>();
    let host = generator.subschema_for::<HostFrame<PlakExtension>>();
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "plak wire v2",
        "wire_version": WIRE_VERSION,
        "binary_id": crate::build_identity::known_binary_id(),
        "client_frame": client,
        "host_frame": host,
        "$defs": generator.take_definitions(true),
    })
}

/// Prints [`document`] as one pretty JSON document.
///
/// # Errors
///
/// A lost stdout.
pub(crate) fn print(stdout: &mut dyn Write) -> CliResult<ProcessOutcome> {
    let text = serde_json::to_string_pretty(&document())
        .map_err(|_| CliFailure::new("plak wire schema could not encode its schema"))?;
    writeln!(stdout, "{text}").map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod variant_tests;
