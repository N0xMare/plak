use std::io::{self as stdio, Write as _};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::Serialize;

use crate::shipping_artifact::{self as shipping, io};
use crate::{CliFailure, CliResult, ProcessOutcome};

mod apply;
mod artifacts;
pub(crate) mod canary;
mod evidence;
mod fleet;
mod inventory;
mod plan;
mod probe;
mod protocol;
mod receipt;
mod ssh;
mod target;
mod timing;

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
    stage: inventory::Stage,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Verb {
    Plan,
    Apply,
    Status,
}

impl Verb {
    fn name(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Apply => "apply",
            Self::Status => "status",
        }
    }
}

type Result<T> = std::result::Result<T, Halt>;

#[derive(Debug)]
struct Halt {
    result: &'static str,
    message: String,
}

impl Halt {
    fn blocked(message: &str) -> Self {
        Self {
            result: "blocked",
            message: message.to_owned(),
        }
    }

    fn reconcile(message: &str) -> Self {
        Self {
            result: "needs_reconcile",
            message: message.to_owned(),
        }
    }

    fn bootstrap() -> Self {
        Self {
            result: "bootstrap_required",
            message: plan::BOOTSTRAP.to_owned(),
        }
    }
}

impl From<CliFailure> for Halt {
    fn from(error: CliFailure) -> Self {
        Self::blocked(error.message())
    }
}

#[derive(Default)]
struct Report(serde_json::Map<String, serde_json::Value>);

impl Report {
    fn put(&mut self, name: &str, value: &impl Serialize) -> CliResult<()> {
        let value = serde_json::to_value(value)
            .map_err(|_| CliFailure::new("cannot encode deploy report"))?;
        self.0.insert(name.to_owned(), value);
        Ok(())
    }

    fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.0.get(key)
    }

    fn remove(&mut self, key: &str) {
        self.0.remove(key);
    }

    fn value(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.0
    }
}

pub(crate) fn target() -> CliResult<ProcessOutcome> {
    target::serve(&mut stdio::stdin().lock(), &mut stdio::stdout().lock())
}

pub(crate) fn run(action: Action) -> CliResult<ProcessOutcome> {
    let (verb, options) = match action {
        Action::Plan(options) => (Verb::Plan, options),
        Action::Apply(options) => (Verb::Apply, options),
        Action::Status(options) => (Verb::Status, options),
    };
    let mut report = Report::default();
    report.put("schema", &evidence::SCHEMA)?;
    report.put("stage", &options.stage)?;
    report.put("verb", &verb.name())?;
    let inventory_path = options.inventory.to_string_lossy();
    report.put(
        "retry",
        &shell_words::join([
            "plakd",
            "deploy",
            "status",
            "--inventory",
            &inventory_path,
            "--stage",
            options.stage.name(),
        ]),
    )?;
    let result = execute(verb, &options, &mut report);
    if let Err(error) = &result {
        report.put("result", &error.result)?;
        report.put("reason", &error.message)?;
    }
    stdio::stdout()
        .lock()
        .write_all(&shipping::canonical(report.value())?)
        .map_err(io)?;
    result.map_err(|_| CliFailure::new("deploy halted; see the JSON report"))?;
    Ok(ProcessOutcome::Success)
}

fn execute(verb: Verb, options: &Options, report: &mut Report) -> Result<()> {
    let inventory = inventory::Inventory::read(&options.inventory)?;
    let configuration = inventory.select(options.stage)?;
    let artifacts = artifacts::Artifacts::import(&inventory, &configuration)?;
    report.put("revision", &inventory.revision)?;
    report.put("manifests", &artifacts.pins)?;
    let fleet = fleet::Remote::new(options.stage, &artifacts, configuration.host)?;
    let mut key = shipping::canonical(&(options.stage, &configuration, &artifacts.pins))?;
    key.pop();
    let intent = shipping::digest(&key);
    let deployment = apply::Deploy {
        artifacts: &artifacts,
        config: &configuration,
        fleet,
        stage: options.stage,
        intent,
    };
    work(&deployment, verb, configuration.evidence, report)
}

fn work(
    deployment: &apply::Deploy<'_, impl fleet::Fleet>,
    verb: Verb,
    path: &Path,
    report: &mut Report,
) -> Result<()> {
    if verb == Verb::Apply {
        report.put("operation", &uuid::Uuid::new_v4())?;
        let mut log = evidence::Log::open(path, true)?;
        let result = deployment.apply(&mut log, report);
        if let Err(error) = &result {
            #[derive(Serialize)]
            struct Failed<'a> {
                step: Option<&'a serde_json::Value>,
                result: &'a str,
            }
            log.record(
                &deployment.intent,
                "halt",
                &Failed {
                    step: report.get("step"),
                    result: error.result,
                },
            )?;
        }
        return result;
    }
    let rows = evidence::read(path)?;
    if verb == Verb::Status {
        if let Some(last) = rows.last() {
            report.put("last_evidence", last)?;
        }
        if evidence::pending(&rows)?.is_some() {
            let mut log = evidence::Log::open(path, false)?;
            deployment.plan(&log.rows.clone(), Some(&mut log), report)?;
            if let Some(last) = log.rows.last() {
                report.put("last_evidence", last)?;
            }
            report.put("result", &"ready")?;
            return Ok(());
        }
    }
    deployment.plan(&rows, None, report)?;
    report.put("result", &"ready")?;
    Ok(())
}
