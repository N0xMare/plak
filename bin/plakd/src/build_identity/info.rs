use std::io::Write;

use plakous::gateway::protocol::SUPPORTED_VERSIONS;
use plakous_host_protocol::wire::WIRE_VERSION;
use serde::Serialize;

use crate::{CliFailure, CliResult, ProcessOutcome, carry, commands::runner, home, wire_schema};

#[derive(Serialize)]
struct BuildInfo<'a> {
    export: Export,
    flavor: String,
    gateway: Versions<&'static [u16]>,
    host_upgrade: HostUpgrade,
    omitted_features: &'static [&'static str],
    product_version: &'static str,
    revision: String,
    runner: Runner<'a>,
    state_key: String,
    test_hooks: bool,
    wire: Wire,
}

#[derive(Serialize)]
struct Export {
    read: &'static [u32],
    write: u32,
}

#[derive(Serialize)]
struct Versions<T> {
    served: T,
    spoken: T,
}

#[derive(Serialize)]
struct HostUpgrade {
    coordinator: u32,
    receipt: u32,
}

#[derive(Serialize)]
struct Runner<'a> {
    protocol: u32,
    retention: Versions<&'a [u32]>,
}

#[derive(Serialize)]
struct Wire {
    number: u32,
    schema_pin: String,
}

pub(crate) fn print(out: &mut dyn Write) -> CliResult<ProcessOutcome> {
    let version = runner::version();
    let retention = runner::retention_versions();
    let schema_pin = wire_schema::pin().map_err(|error| CliFailure::new(error.to_string()))?;
    let info = BuildInfo {
        export: Export {
            read: &carry::IMPORTABLE_VERSIONS,
            write: carry::VERSION,
        },
        flavor: version.flavor,
        gateway: Versions {
            served: SUPPORTED_VERSIONS,
            spoken: SUPPORTED_VERSIONS,
        },
        host_upgrade: HostUpgrade {
            coordinator: carry::upgrade::PROTOCOL,
            receipt: carry::upgrade::receipt::VERSION,
        },
        omitted_features: &[],
        product_version: plakous::VERSION,
        revision: version.source_revision,
        runner: Runner {
            protocol: version.protocol,
            retention: Versions {
                served: &retention,
                spoken: &retention,
            },
        },
        state_key: home::state::state_key(),
        test_hooks: super::TEST_HOOKS,
        wire: Wire {
            number: WIRE_VERSION,
            schema_pin,
        },
    };
    let stdout_failed = |_| CliFailure::new("stdout failed");
    let mut document = serde_json::to_value(info).map_err(stdout_failed)?;
    document.sort_all_objects();
    serde_json::to_writer(&mut *out, &document).map_err(stdout_failed)?;
    writeln!(out).map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}
