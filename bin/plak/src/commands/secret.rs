//! `plak secret set|rm|list`: named secrets that configuration refers to by
//! name (`[mcp_servers.<id>]` `secret_env`, `bearer_secret`,
//! `secret_headers`). Values are read from a hidden prompt, never from argv,
//! and never printed.

use std::io;

use crate::home::paths::PlakPaths;
use crate::home::secrets;
use crate::terminal_application::read_terminal_secret_line;
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};

/// `plak secret set <name>`: hidden read, then a 0600 file.
pub fn secret_set(
    name: &str,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    secrets::validate_secret_name(name).map_err(CliFailure::from)?;
    let paths = PlakPaths::from_env().map_err(CliFailure::from)?;
    write!(stderr, "value for secret {name} (input hidden): ")
        .and_then(|()| stderr.flush())
        .map_err(|_| CliFailure::new("stderr failed"))?;
    let value = read_terminal_secret_line(stdin, stderr, secrets::MAX_NAMED_SECRET_BYTES + 2)
        .map_err(|_| CliFailure::new("could not read the secret value"))?;
    secrets::set_named_secret(&paths, name, value.as_str()).map_err(CliFailure::from)?;
    writeln!(stderr, "stored secret {name}").map_err(|_| CliFailure::new("stderr failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak secret rm <name>`.
pub fn secret_rm(name: &str, stdout: &mut dyn io::Write) -> CliResult<ProcessOutcome> {
    let paths = PlakPaths::from_env().map_err(CliFailure::from)?;
    let removed = secrets::remove_named_secret(&paths, name).map_err(CliFailure::from)?;
    let line = if removed {
        format!("removed secret {name}")
    } else {
        format!("no secret named {name}")
    };
    writeln!(stdout, "{line}").map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak secret list`: one name per line, never a value.
pub fn secret_list(stdout: &mut dyn io::Write) -> CliResult<ProcessOutcome> {
    let paths = PlakPaths::from_env().map_err(CliFailure::from)?;
    for name in secrets::list_named_secrets(&paths).map_err(CliFailure::from)? {
        writeln!(stdout, "{name}").map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}
