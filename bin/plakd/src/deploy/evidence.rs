use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::CliResult;
use crate::shipping_artifact::{self as shipping, check, io};

use super::protocol::{
    AccountIdentity, Dependencies, GatewayIntent, GatewayObserved, Health, RunnerObserved,
    SourceTransition, StageIdentity,
};
use super::{Halt, Result};

pub(super) const SCHEMA: u32 = 5;

pub(super) fn isolate_staging(
    targets: &BTreeSet<AccountIdentity>,
    production_log: Option<&Path>,
    receipt: &Path,
) -> Result<()> {
    let linux: BTreeSet<_> = targets
        .iter()
        .filter(|node| node.platform.is_linux())
        .map(|node| node.machine_sha256.as_str())
        .collect();
    if linux.is_empty() {
        return Ok(());
    }
    let matches = |row: &serde_json::Value| {
        let digest = |account: &serde_json::Value| {
            account["machine_sha256"]
                .as_str()
                .is_some_and(|sha| linux.contains(sha))
        };
        digest(&row["stage_identity"])
            || ["target_accounts", "accounts"].iter().any(|field| {
                row[field]
                    .as_array()
                    .is_some_and(|accounts| accounts.iter().any(digest))
            })
    };
    let log = production_log
        .map(|path| history(path, 32 * shipping::LIMIT))
        .transpose()?
        .unwrap_or_default();
    if !log.is_empty() && !log.ends_with('\n') {
        return Err(Halt::reconcile(
            "local production evidence is incomplete; reconcile before staging",
        ));
    }
    for line in log.split_terminator('\n') {
        let row: serde_json::Value = serde_json::from_str(line).map_err(|_| {
            Halt::reconcile("cannot read local production evidence for staging isolation")
        })?;
        if matches(&row) {
            return Err(Halt::blocked(
                "staging Linux machine matches local production evidence",
            ));
        }
    }
    let raw = history(receipt, shipping::LIMIT)?;
    if !raw.is_empty() {
        let row: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|_| Halt::reconcile("cannot read local receipt for staging isolation"))?;
        if row["stage"] == "production" && matches(&row) {
            return Err(Halt::blocked(
                "staging Linux machine matches a local production receipt",
            ));
        }
    }
    Ok(())
}

fn history(path: &Path, bound: u64) -> Result<String> {
    if fs::symlink_metadata(path).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound) {
        return Ok(String::new());
    }
    let file = shipping::open(path)?;
    private(&file)?;
    let mut raw = String::new();
    file.take(bound + 1).read_to_string(&mut raw).map_err(io)?;
    if raw.len() as u64 > bound {
        return Err(Halt::reconcile(
            "operator production history exceeds its read bound",
        ));
    }
    Ok(raw)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Cohort {
    Runner,
    RunnerWithdrawal,
    Gateway,
    GatewayReady,
    Host,
    Clients,
}

impl Cohort {
    pub(super) fn phase(self) -> &'static str {
        match self {
            Self::Runner => "runner_intent",
            Self::RunnerWithdrawal => "runner_withdrawal",
            Self::Gateway => "gateway_intent",
            Self::GatewayReady => "gateway_ready",
            Self::Host => "upgrade_intent",
            Self::Clients => "client_intent",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Row {
    schema: u32,
    pub(super) intent: String,
    time: u64,
    pub(super) phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_identity: Option<StageIdentity>,
    #[serde(flatten)]
    pub(super) fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    pub(super) cohort: Cohort,
    pub(super) ticket: uuid::Uuid,
    pub(super) instance: String,
    pub(super) source_sha256: String,
    pub(super) source_transition: SourceTransition,
    pub(super) dependencies: Dependencies,
    pub(super) baseline: Health,
    pub(super) accounts: BTreeSet<AccountIdentity>,
    pub(super) runners: BTreeMap<String, RunnerObserved>,
    pub(super) host_upgrade: bool,
    pub(super) gateway: Option<GatewayIntent>,
    pub(super) gateway_successor: Option<GatewayObserved>,
    pub(super) gateway_runner_withdrawal: bool,
}

pub(super) struct Log {
    file: File,
    pub(super) rows: Vec<Row>,
    pub(super) stage_identity: Option<StageIdentity>,
}

impl Log {
    pub(super) fn open(path: &Path, create: bool) -> Result<Self> {
        use rustix::fs::{Mode, OFlags};
        let mut flags =
            OFlags::RDWR | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        flags.set(OFlags::CREATE, create);
        let mut file = File::from(
            rustix::fs::open(path, flags, Mode::RUSR | Mode::WUSR)
                .map_err(std::io::Error::from)
                .map_err(io)?,
        );
        private(&file)?;
        file.try_lock()
            .map_err(|_| Halt::blocked("another deploy holds the evidence log"))?;
        let rows = parse(&mut file)?;
        file.seek(std::io::SeekFrom::End(0)).map_err(io)?;
        Ok(Self {
            file,
            rows,
            stage_identity: None,
        })
    }

    pub(super) fn record(
        &mut self,
        intent: &str,
        phase: &str,
        fields: &impl Serialize,
    ) -> CliResult<()> {
        let fields = if let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(fields) {
            fields
        } else {
            serde_json::Map::new()
        };
        let row = Row {
            schema: SCHEMA,
            intent: intent.to_owned(),
            time: now(),
            phase: phase.to_owned(),
            stage_identity: self.stage_identity.clone(),
            fields,
        };
        self.file
            .write_all(&shipping::canonical(&row)?)
            .map_err(io)?;
        self.file.sync_data().map_err(io)?;
        self.rows.push(row);
        Ok(())
    }
}

pub(super) fn read(path: &Path) -> Result<Vec<Row>> {
    if fs::symlink_metadata(path).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound) {
        return Ok(Vec::new());
    }
    let mut file = shipping::open(path)?;
    private(&file)?;
    parse(&mut file)
}

fn parse(file: &mut File) -> Result<Vec<Row>> {
    let mut data = Vec::new();
    file.take(32 * shipping::LIMIT + 1)
        .read_to_end(&mut data)
        .map_err(io)?;
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if data.len() as u64 > 32 * shipping::LIMIT || !data.ends_with(b"\n") {
        return Err(Halt::reconcile(
            "incomplete or oversized evidence log; reconcile before proceeding",
        ));
    }
    data[..data.len() - 1]
        .split(|byte| *byte == b'\n')
        .map(|line| {
            let row: Row = serde_json::from_slice(line).map_err(|_| {
                Halt::reconcile("invalid evidence record; reconcile before proceeding")
            })?;
            if row.schema != SCHEMA {
                return Err(Halt::reconcile(
                    "unrecognized evidence schema; reconcile before proceeding",
                ));
            }
            Ok(row)
        })
        .collect()
}

pub(super) fn pending(rows: &[Row]) -> Result<Option<(&str, Pending)>> {
    for row in rows.iter().rev() {
        match row.phase.as_str() {
            "verified" | "withdrawn" => return Ok(None),
            "runner_intent" | "runner_withdrawal" | "gateway_intent" | "gateway_ready"
            | "upgrade_intent" | "client_intent" => {
                let pending: Pending = serde_json::from_value(serde_json::Value::Object(
                    row.fields.clone(),
                ))
                .map_err(|_| Halt::reconcile("recorded cohort intent lacks required facts"))?;
                if pending.cohort.phase() != row.phase {
                    return Err(Halt::reconcile("recorded cohort and evidence phase differ"));
                }
                return Ok(Some((&row.intent, pending)));
            }
            _ => {}
        }
    }
    Ok(None)
}

pub(super) fn private(file: &File) -> CliResult<()> {
    let meta = file.metadata().map_err(io)?;
    check(
        meta.is_file()
            && meta.uid() == rustix::process::geteuid().as_raw()
            && meta.mode().trailing_zeros() >= 6,
        "receipt or evidence must be an owned private regular file",
    )
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_secs())
}
