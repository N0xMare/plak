use std::io::Write;

use plakous::gateway::protocol::SUPPORTED_VERSIONS;
use plakous::runner::protocol::retention;
use plakous_host_protocol::wire::WIRE_VERSION;
use serde_json::json;

use crate::{CliFailure, CliResult, ProcessOutcome, carry, home, wire_schema};

pub(crate) fn print(out: &mut dyn Write) -> CliResult<ProcessOutcome> {
    let runner = crate::commands::runner::version();
    let retention_versions: Vec<_> = (1..=retention::VERSION)
        .filter(|version| retention::admits(*version))
        .collect();
    let pin = wire_schema::pin().map_err(|error| CliFailure::new(error.to_string()))?;
    let mut document = json!({
        "export": { "read": carry::IMPORTABLE_VERSIONS, "write": carry::VERSION },
        "flavor": runner.flavor,
        "gateway": { "served": SUPPORTED_VERSIONS, "spoken": SUPPORTED_VERSIONS },
        "host_upgrade": {
            "coordinator": carry::upgrade::PROTOCOL,
            "receipt": carry::upgrade::receipt::VERSION,
        },
        "product_version": plakous::VERSION,
        "revision": runner.source_revision,
        "runner": {
            "protocol": runner.protocol,
            "retention": { "served": retention_versions, "spoken": retention_versions },
        },
        "state_key": home::state::state_key(),
        "wire": { "number": WIRE_VERSION, "schema_pin": pin },
    });
    document.sort_all_objects();
    let stdout_failed = |_| CliFailure::new("stdout failed");
    serde_json::to_writer(&mut *out, &document).map_err(stdout_failed)?;
    writeln!(out).map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}
