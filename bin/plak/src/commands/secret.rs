//! `plak secret set|rm|list`: named secrets that configuration refers to by
//! name (`[mcp_servers.<id>]` `secret_env`, `bearer_secret`,
//! `secret_headers`), and `plak runner secret set|rm|list`: this machine's
//! runner secrets, which a server placed on its runner reads by the same
//! names. Values are read from a hidden prompt, never from argv, and never
//! printed.

use std::io;

use crate::home::paths::{PlakPaths, SecretShelf};
use crate::home::secrets;
use crate::terminal_application::read_terminal_secret_line;
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};

/// `plak [runner] secret set <name>`: hidden read, then a 0600 file.
pub fn secret_set(
    paths: &PlakPaths,
    shelf: SecretShelf,
    name: &str,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    secrets::validate_secret_name(name).map_err(CliFailure::from)?;
    write!(stderr, "value for secret {name} (input hidden): ")
        .and_then(|()| stderr.flush())
        .map_err(|_| CliFailure::new("stderr failed"))?;
    let value = read_terminal_secret_line(stdin, stderr, secrets::MAX_NAMED_SECRET_BYTES + 2)
        .map_err(|_| CliFailure::new("could not read the secret value"))?;
    secrets::set_shelf_secret(paths, shelf, name, value.as_str()).map_err(CliFailure::from)?;
    writeln!(stderr, "stored {} {name}", noun(shelf))
        .map_err(|_| CliFailure::new("stderr failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak [runner] secret rm <name>`.
pub fn secret_rm(
    paths: &PlakPaths,
    shelf: SecretShelf,
    name: &str,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let removed = secrets::remove_shelf_secret(paths, shelf, name).map_err(CliFailure::from)?;
    let noun = noun(shelf);
    let line = if removed {
        format!("removed {noun} {name}")
    } else {
        format!("no {noun} named {name}")
    };
    writeln!(stdout, "{line}").map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak [runner] secret list`: one name per line, never a value.
pub fn secret_list(
    paths: &PlakPaths,
    shelf: SecretShelf,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    for name in secrets::list_shelf_secrets(paths, shelf).map_err(CliFailure::from)? {
        writeln!(stdout, "{name}").map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

const fn noun(shelf: SecretShelf) -> &'static str {
    match shelf {
        SecretShelf::Named => "secret",
        SecretShelf::Runner => "runner secret",
        SecretShelf::Web => "web secret",
    }
}
