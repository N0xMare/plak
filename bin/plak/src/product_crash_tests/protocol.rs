use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub(super) use plakous_offline_crash_harness::protocol::{
    BoundaryEvidenceKind, CrashBoundary, PROTOCOL_VERSION,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProductWorkflow {
    RootDelivery,
    SendNow,
    Fork,
    Rebind,
    Compact,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProductCrashBoundary {
    Prepared,
    RuntimeReturned,
    ProductPublished,
    ProductCommitted,
    Finalization,
    CompletedToolRound,
    FinalizationPendingTerminal,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ControlCommand {
    ProductRun {
        version: u16,
        home: PathBuf,
        workspace: PathBuf,
        state: PathBuf,
        workflow: ProductWorkflow,
        cut: ProductCrashBoundary,
    },
    ProductProbe {
        version: u16,
        home: PathBuf,
        state: PathBuf,
    },
    ProductRecover {
        version: u16,
        home: PathBuf,
        workspace: PathBuf,
        state: PathBuf,
    },
    ActualHostRun {
        version: u16,
        home: PathBuf,
        workspace: PathBuf,
        state: PathBuf,
        workflow: ProductWorkflow,
        cut: ProductCrashBoundary,
    },
    ActualHostRecover {
        version: u16,
        home: PathBuf,
        workspace: PathBuf,
        state: PathBuf,
        next_prompt: bool,
    },
}

impl ControlCommand {
    pub(super) fn version(&self) -> u16 {
        match self {
            Self::ProductRun { version, .. }
            | Self::ProductProbe { version, .. }
            | Self::ProductRecover { version, .. }
            | Self::ActualHostRun { version, .. }
            | Self::ActualHostRecover { version, .. } => *version,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ChildReply {
    Started {
        version: u16,
    },
    Boundary {
        version: u16,
        boundary: CrashBoundary,
        evidence_kind: BoundaryEvidenceKind,
        evidence_sequence: u64,
    },
    ProductBoundary {
        version: u16,
        workflow: ProductWorkflow,
        boundary: ProductCrashBoundary,
    },
    ProductProbed {
        version: u16,
        report: Box<ProductProbeReport>,
    },
    Failed {
        version: u16,
        message: String,
    },
}

pub(super) fn decode_reply(frame: &str) -> Result<ChildReply, String> {
    if let Ok(reply) = serde_json::from_str(frame) {
        return Ok(reply);
    }
    match serde_json::from_str::<plakous_offline_crash_harness::protocol::ChildReply>(frame)
        .map_err(|error| error.to_string())?
    {
        plakous_offline_crash_harness::protocol::ChildReply::Boundary {
            version,
            boundary,
            evidence_kind,
            evidence_sequence,
        } => Ok(ChildReply::Boundary {
            version,
            boundary,
            evidence_kind,
            evidence_sequence,
        }),
        plakous_offline_crash_harness::protocol::ChildReply::Failed { version, message } => {
            Ok(ChildReply::Failed { version, message })
        }
        _ => Err("unexpected facade-harness reply in product child".to_owned()),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ProductProbeReport {
    pub(super) workflow: ProductWorkflow,
    pub(super) product_session_id: String,
    pub(super) locator: Option<String>,
    pub(super) queue_items: usize,
    pub(super) queue_paused: bool,
    pub(super) target_product_session_id: Option<String>,
    pub(super) target_locator: Option<String>,
    pub(super) source_product_session_id: Option<String>,
    pub(super) source_workflow_operation_id: Option<String>,
    pub(super) provider_dispatches: usize,
    pub(super) effect_dispatches: usize,
}

pub(super) fn write_reply(reply: &ChildReply) -> io::Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(super::REPLY_PREFIX.as_bytes())?;
    serde_json::to_writer(&mut output, reply).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

pub(super) fn write_fsynced_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("tmp");
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()
}
