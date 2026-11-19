use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

use crate::{CliFailure, CliResult, ProcessOutcome};

pub(crate) mod canary;

#[derive(Clone, Debug, Eq, PartialEq, Subcommand)]
pub(crate) enum Action {
    Plan(Options),
    Apply(Options),
    Status(Options),
}

#[derive(Clone, Debug, Eq, PartialEq, Args)]
pub(crate) struct Options {
    #[arg(long)]
    inventory: PathBuf,
    #[arg(long)]
    stage: Stage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Stage {
    Staging,
    Production,
}

pub(crate) fn run(action: Action) -> CliResult<ProcessOutcome> {
    let (verb, options) = match action {
        Action::Plan(options) => ("plan", options),
        Action::Apply(options) => ("apply", options),
        Action::Status(options) => ("status", options),
    };
    let stage = match options.stage {
        Stage::Staging => "staging",
        Stage::Production => "production",
    };
    let status = std::process::Command::new("python3")
        .args(["-I", "-c", include_str!("deploy/driver.py")])
        .arg(include_str!("deploy/target.py"))
        .args([verb, stage])
        .arg(options.inventory)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|_| CliFailure::new("deploy requires Python 3.11+ on the operator machine"))?;
    if status.success() {
        Ok(ProcessOutcome::Success)
    } else {
        Err(CliFailure::new("deploy halted; see the JSON report"))
    }
}
