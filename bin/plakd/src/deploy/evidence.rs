use std::fs::{self, File};
use std::io::{Read, Seek as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::CliResult;
use crate::shipping_artifact::{self as shipping, check, io};

use super::protocol::{Dependencies, Health, SourceTransition, StageIdentity};
use super::{Halt, Result};

pub(super) const SCHEMA: u32 = 2;

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
pub(super) struct Pending {
    pub(super) instance: String,
    pub(super) source_sha256: String,
    pub(super) source_transition: SourceTransition,
    pub(super) dependencies: Dependencies,
    pub(super) baseline: Health,
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
            "upgrade_intent" => {
                let pending = serde_json::from_value(serde_json::Value::Object(row.fields.clone()))
                    .map_err(|_| Halt::reconcile("recorded upgrade intent lacks required facts"))?;
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
