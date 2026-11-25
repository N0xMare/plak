//! `plakd wire schema`: the supported JSON Schema (draft 2020-12) of this
//! build's wire v2 frames, both directions, with Plak's extension traffic,
//! and the protocol version and binary they belong to. A client checks its
//! own frames against the exact build it speaks to.

use std::io::Write;

use plakd_wire_types::v13::schema;
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use serde_json::Value;

/// The schema document: `client_frame` and `host_frame` reference their
/// definitions under `$defs`.
#[must_use]
pub(crate) fn document() -> Value {
    schema(Some(crate::build_identity::binary_id()))
}

pub(crate) fn pin() -> Result<String, serde_json::Error> {
    normalized_pin(schema(None))
}

fn normalized_pin(mut document: Value) -> Result<String, serde_json::Error> {
    if let Some(fields) = document.as_object_mut() {
        fields.remove("binary_id");
    }
    document.sort_all_objects();
    let bytes = serde_json::to_vec(&document)?;
    Ok(plakous::model::sha256_hex(&[&bytes]))
}

/// Prints the serving schema as one pretty JSON document.
///
/// # Errors
///
/// A lost stdout.
pub(crate) fn print(stdout: &mut dyn Write) -> CliResult<ProcessOutcome> {
    let schema = document();
    let text = serde_json::to_string_pretty(&schema)
        .map_err(|_| CliFailure::new("plakd wire schema could not encode its schema"))?;
    writeln!(stdout, "{text}").map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod variant_tests;
